//! JITビュー (F1a、ADR-0008) — 焼き候補ブロックを外の生成器へ渡す口。
//!
//! coreの内部表現 (Uop) は公開しない。ここにあるのは**coreが「JITで焼いてよい」
//! と認めた命令だけ**の中立なIRで、認めない命令 (メモリに触る形・特権命令…) に
//! 当たったらブロックはそこで終わる。生成器 (wasmシェル/ネイティブランナー) は
//! このIRとフィールド実アドレス表 ([`layout`]) だけを見て動く — coreの無依存と
//! 「意味論の原本はインタプリタ」の規律はこの境界で守られる。
//!
//! F1aの対象はレジスタとフラグしか触らない命令に限る:
//! #PFが起き得ない = 巻き戻しが要らない = 生成コードに脱出点が要らないため、
//! 最初の骨格から失敗系を締め出せる。
//!
//! F1b-1でメモリ**ロード**が加わった。フォールト脱出モデル (ADR-0008):
//! ロードの前にヘルパでtranslateを試し、フォールトしそうなら状態を1つも
//! 変えずにブロックを脱出する — 巻き戻しは相変わらず要らない。

use super::{decode, MemRef, Rm, Uop};
use crate::Machine;

/// メモリオペランドの作り方 (JITの語彙)。coreの [`MemRef`] の公開鏡 —
/// 実効アドレスは生成コードがレジスタの**今の**値から組み、セグメント適用と
/// 変換はヘルパ (Rust) がやる
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitMem {
    /// 基底レジスタ (-1 = 無し)
    pub base: i8,
    /// インデックスレジスタ (-1 = 無し)
    pub index: i8,
    pub scale: u8,
    /// セグメント (デコード時に上書き規則まで解決済み)
    pub seg: u8,
    pub disp: u32,
}

impl From<MemRef> for JitMem {
    fn from(m: MemRef) -> Self {
        JitMem {
            base: m.base,
            index: m.index,
            scale: m.scale,
            seg: m.seg,
            disp: m.disp,
        }
    }
}

/// JITの語彙: レジスタ間 (F1a) + メモリロード (F1b-1) + ストア/RMW (F1b-2)。
///
/// ストアが入っても「ブロック内で世代 (page_gen) が動かない」前提は保たれる —
/// 現行coreで世代を進めるのは REP文字列 (note_write_range) と write_phys8 だけで、
/// 素の線形ストアは進めない (JITヘルパはこれを正確に写す)。REPは語彙に無い。
/// この前提が変わる (素のストアが世代を進めるようになる) ときは、
/// 「ヘルパが自ページの世代を進めたら実行済みn+1で脱出」の受けを足すこと。
///
/// フラグの扱いは lazy flags (cc_*) の材料更新として生成する —
/// C1で作ったcc_op方式がそのままJITのフラグモデルになる。
/// RMWだけは例外で、ccの更新ごとRustヘルパ (alu_w) の中で済む
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitOp {
    /// mov r32, imm32
    MovRI {
        dst: u8,
        imm: u32,
    },
    /// mov r32, r32
    MovRR {
        dst: u8,
        src: u8,
    },
    /// ALU r32, r32 — dst = alu(kind, dst, src)。kind7 (CMP) はdstを書かない
    AluRR {
        kind: u8,
        dst: u8,
        src: u8,
    },
    /// ALU r32, imm32 (0x83の符号拡張は済んでいる)
    AluRI {
        kind: u8,
        dst: u8,
        imm: u32,
    },
    /// test r32, r32 (フラグだけ)
    TestRR {
        a: u8,
        b: u8,
    },
    /// inc/dec r32 (CFは不変)
    IncDec {
        reg: u8,
        dec: bool,
    },
    /// lea r32, [base + index<<scale + disp] — 計算だけ、メモリに触らない
    Lea {
        dst: u8,
        base: i8,
        index: i8,
        scale: u8,
        disp: u32,
    },
    /// mov r32, [mem] (F1b-1。フォールトしそうなら脱出)
    MovRM {
        dst: u8,
        mem: JitMem,
    },
    /// ALU r32, [mem] — dst = alu(kind, dst, mem)。kind7 (CMP) はdstを書かない
    AluRM {
        kind: u8,
        dst: u8,
        mem: JitMem,
    },
    /// cmp [mem], r32 (ALUグリッドのrm=dst形でロードだけで済むkind7)
    CmpMR {
        mem: JitMem,
        reg: u8,
    },
    /// cmp [mem], imm32 (Grp1のkind7 — 同上)
    CmpMI {
        mem: JitMem,
        imm: u32,
    },
    /// test [mem], r32 (フラグだけ)
    TestMR {
        mem: JitMem,
        reg: u8,
    },
    /// mov [mem], r32 (F1b-2。フォールトしそうなら書く前に脱出)
    StoreMR {
        mem: JitMem,
        src: u8,
    },
    /// mov [mem], imm32
    StoreMI {
        mem: JitMem,
        imm: u32,
    },
    /// ALU [mem], r32 — read→alu→write をRustヘルパ1呼びで (kind7は来ない)
    AluMR {
        kind: u8,
        mem: JitMem,
        reg: u8,
    },
    /// ALU [mem], imm32
    AluMI {
        kind: u8,
        mem: JitMem,
        imm: u32,
    },
    /// push r32 (F1b-3。SP更新前に脱出できる — push32と同じ「成功時だけ確定」)
    PushR {
        src: u8,
    },
    /// push imm32
    PushI {
        imm: u32,
    },
    /// pop r32
    PopR {
        dst: u8,
    },
    /// leave (SP←BP、BP←pop)
    Leave,
    /// xchg eAX, r32 (レジスタ間 — 0x90 nop = 自分と交換も含む)
    XchgA {
        reg: u8,
    },
    /// 終端: 条件分岐 (取られたら ip += rel は命令長込みの相対)
    Jcc {
        cc: u8,
        rel: u32,
    },
    /// 終端: 無条件相対ジャンプ
    Jmp {
        rel: u32,
    },
    /// 終端: call rel — 戻り番地 (頭+ここまでの長さ) をpushしてから ip += len+rel。
    /// pushが脱出点 (F1b-3)
    CallRel {
        rel: u32,
    },
    /// 終端: ret — pop した値が次のip。popが脱出点
    Ret,
    // ---- ここから語彙v2 (F1c-b2、CAP_VOCAB2)。wasm生成器 (凍結) には
    //      collectのcapsで渡らない — ネイティブ専用の拡張 ----
    /// shift/rot r32 (imm形)。フラグはヘルパ内で完結 (shift_rotの意味論は
    /// eager — cc材料の形が違うので畳まない。#PF不能なので脱出も不要)
    ShiftRI {
        kind: u8,
        reg: u8,
        count: u8,
    },
    /// shift/rot r32, CL。countは生成コードがCLから読んで渡す
    ShiftRC {
        kind: u8,
        reg: u8,
    },
    /// movzx r32, r8 (レジスタ形。src8は8bitレジスタ番号 — 4..7はAH形)
    MovzxBR {
        dst: u8,
        src8: u8,
    },
    /// movzx r32, [m8]
    MovzxBM {
        dst: u8,
        mem: JitMem,
    },
    /// movzx r32, r16 (低16bit)
    MovzxWR {
        dst: u8,
        src: u8,
    },
    /// movzx r32, [m16]
    MovzxWM {
        dst: u8,
        mem: JitMem,
    },
    /// ALU r8, r8 (dst8/src8は8bitレジスタ番号)。kind7 (CMP) はdstを書かない
    Alu8RR {
        kind: u8,
        dst8: u8,
        src8: u8,
    },
    /// ALU r8, imm8
    Alu8RI {
        kind: u8,
        dst8: u8,
        imm: u8,
    },
    /// ALU r8, [m8] — dst8 = alu(dst8, mem)。ロードが脱出点
    Alu8RM {
        kind: u8,
        dst8: u8,
        mem: JitMem,
    },
    /// cmp [m8], r8 / imm8 (ロードだけで済むkind7)
    Cmp8MR {
        mem: JitMem,
        reg8: u8,
    },
    Cmp8MI {
        mem: JitMem,
        imm: u8,
    },
    /// test r8, r8 / test [m8], r8 (フラグだけ)
    Test8RR {
        a8: u8,
        b8: u8,
    },
    Test8MR {
        mem: JitMem,
        reg8: u8,
    },
    /// F6 kind0-3 (test imm/not/neg) のレジスタ形。NEGのCF上書きが
    /// 遅延材料に畳めないのでヘルパで完結 (#PF不能・脱出不要)
    Grp3b8R {
        kind: u8,
        reg8: u8,
        imm: u8,
    },
    /// mov r8, r8 / mov r8, [m8]
    Mov8RR {
        dst8: u8,
        src8: u8,
    },
    Mov8RM {
        dst8: u8,
        mem: JitMem,
    },
}

/// 語彙の世代 (collectに渡す)。wasm生成器は凍結時点のF1B、ネイティブはV2
pub const CAP_F1B: u32 = 0;
pub const CAP_VOCAB2: u32 = 1;
/// F1c-c: jccを終端にせず、**不成立側をブロック内で続ける** (両側焼き)。
/// 成立側は「ipを成立先に書いてk+1で途中退出」— 脱出と同じ形だが完全実行済み。
/// ブロックの最大命令数は変わらない (ループは作らない — 清算の契約 jn を守る)
pub const CAP_CHAIN: u32 = 2;

/// 焼き候補ブロック: 先頭物理アドレスと (命令長, op) の列。
/// 終端は Jcc/Jmp、またはJIT対象外の命令の手前
#[derive(Debug)]
pub struct JitBlock {
    pub head_pa: u32,
    pub ops: Vec<(u8, JitOp)>,
}

/// Uop 1個をJITの語彙へ。(op, これで終端か)。対象外は None
fn convert(u: &Uop, caps: u32) -> Option<(JitOp, bool)> {
    // ---- 語彙v2 (ネイティブ専用)。先に引き受け、外れたらF1B語彙へ ----
    if caps & CAP_VOCAB2 != 0 {
        let v2 = match *u {
            Uop::ShiftRmImm {
                kind,
                rm: Rm::Reg(reg),
                count,
            } => Some(JitOp::ShiftRI { kind, reg, count }),
            Uop::ShiftRmCl {
                kind,
                rm: Rm::Reg(reg),
            } => Some(JitOp::ShiftRC { kind, reg }),
            Uop::MovzxB {
                reg,
                rm: Rm::Reg(src8),
            } => Some(JitOp::MovzxBR { dst: reg, src8 }),
            Uop::MovzxB {
                reg,
                rm: Rm::Mem(mr),
            } => Some(JitOp::MovzxBM {
                dst: reg,
                mem: mr.into(),
            }),
            Uop::MovzxW {
                reg,
                rm: Rm::Reg(src),
            } => Some(JitOp::MovzxWR { dst: reg, src }),
            Uop::MovzxW {
                reg,
                rm: Rm::Mem(mr),
            } => Some(JitOp::MovzxWM {
                dst: reg,
                mem: mr.into(),
            }),
            Uop::Alu8RmR {
                kind,
                rm: Rm::Reg(dst8),
                reg,
            } => Some(JitOp::Alu8RR {
                kind,
                dst8,
                src8: reg,
            }),
            Uop::Alu8RRm {
                kind,
                reg,
                rm: Rm::Reg(src8),
            } => Some(JitOp::Alu8RR {
                kind,
                dst8: reg,
                src8,
            }),
            Uop::Alu8RRm {
                kind,
                reg,
                rm: Rm::Mem(mr),
            } => Some(JitOp::Alu8RM {
                kind,
                dst8: reg,
                mem: mr.into(),
            }),
            // rm=dst形のmem: kind7 (CMP) はロードだけ、他 (RMW8) は語彙外のまま
            Uop::Alu8RmR {
                kind: 7,
                rm: Rm::Mem(mr),
                reg,
            } => Some(JitOp::Cmp8MR {
                mem: mr.into(),
                reg8: reg,
            }),
            Uop::Grp18RmImm {
                kind,
                rm: Rm::Reg(dst8),
                imm,
            } => Some(JitOp::Alu8RI { kind, dst8, imm }),
            Uop::Grp18RmImm {
                kind: 7,
                rm: Rm::Mem(mr),
                imm,
            } => Some(JitOp::Cmp8MI {
                mem: mr.into(),
                imm,
            }),
            Uop::Alu8AImm { kind, imm } => Some(JitOp::Alu8RI { kind, dst8: 0, imm }),
            Uop::Test8RmR {
                rm: Rm::Reg(a8),
                reg,
            } => Some(JitOp::Test8RR { a8, b8: reg }),
            Uop::Test8RmR {
                rm: Rm::Mem(mr),
                reg,
            } => Some(JitOp::Test8MR {
                mem: mr.into(),
                reg8: reg,
            }),
            Uop::Grp3b {
                kind,
                rm: Rm::Reg(reg8),
                imm,
            } if kind < 4 => Some(JitOp::Grp3b8R { kind, reg8, imm }),
            Uop::Mov8RmR {
                rm: Rm::Reg(dst8),
                reg,
            } => Some(JitOp::Mov8RR { dst8, src8: reg }),
            Uop::Mov8RRm {
                reg,
                rm: Rm::Reg(src8),
            } => Some(JitOp::Mov8RR { dst8: reg, src8 }),
            Uop::Mov8RRm {
                reg,
                rm: Rm::Mem(mr),
            } => Some(JitOp::Mov8RM {
                dst8: reg,
                mem: mr.into(),
            }),
            _ => None,
        };
        if let Some(op) = v2 {
            return Some((op, false));
        }
    }
    let op = match *u {
        Uop::MovRImm { reg, imm } => JitOp::MovRI { dst: reg, imm },
        Uop::MovRmR {
            rm: Rm::Reg(dst),
            reg,
        } => JitOp::MovRR { dst, src: reg },
        Uop::MovRRm {
            reg,
            rm: Rm::Reg(src),
        } => JitOp::MovRR { dst: reg, src },
        // ---- F1b-1: メモリロード (書かない形だけ。RMW/ストアはF1b-2) ----
        Uop::MovRRm {
            reg,
            rm: Rm::Mem(mr),
        } => JitOp::MovRM {
            dst: reg,
            mem: mr.into(),
        },
        Uop::AluRRm {
            kind,
            reg,
            rm: Rm::Mem(mr),
        } => JitOp::AluRM {
            kind,
            dst: reg,
            mem: mr.into(),
        },
        // rm=dst形: kind7 (CMP) はロードだけ、他はRMW (F1b-2)
        Uop::AluRmR {
            kind: 7,
            rm: Rm::Mem(mr),
            reg,
        } => JitOp::CmpMR {
            mem: mr.into(),
            reg,
        },
        Uop::AluRmR {
            kind,
            rm: Rm::Mem(mr),
            reg,
        } => JitOp::AluMR {
            kind,
            mem: mr.into(),
            reg,
        },
        Uop::Grp1RmImm {
            kind: 7,
            rm: Rm::Mem(mr),
            imm,
        } => JitOp::CmpMI {
            mem: mr.into(),
            imm,
        },
        Uop::Grp1RmImm {
            kind,
            rm: Rm::Mem(mr),
            imm,
        } => JitOp::AluMI {
            kind,
            mem: mr.into(),
            imm,
        },
        Uop::TestRmR {
            rm: Rm::Mem(mr),
            reg,
        } => JitOp::TestMR {
            mem: mr.into(),
            reg,
        },
        // ---- F1b-2: ストア ----
        Uop::MovRmR {
            rm: Rm::Mem(mr),
            reg,
        } => JitOp::StoreMR {
            mem: mr.into(),
            src: reg,
        },
        Uop::MovRmImm {
            rm: Rm::Mem(mr),
            imm,
        } => JitOp::StoreMI {
            mem: mr.into(),
            imm,
        },
        Uop::MovRmImm {
            rm: Rm::Reg(dst),
            imm,
        } => JitOp::MovRI { dst, imm },
        // ALUの向き: RmR は rm=dst / RRm は reg=dst — sub/cmpで順序が効く
        Uop::AluRmR {
            kind,
            rm: Rm::Reg(dst),
            reg,
        } => JitOp::AluRR {
            kind,
            dst,
            src: reg,
        },
        Uop::AluRRm {
            kind,
            reg,
            rm: Rm::Reg(src),
        } => JitOp::AluRR {
            kind,
            dst: reg,
            src,
        },
        Uop::AluAImm { kind, imm } => JitOp::AluRI { kind, dst: 0, imm },
        Uop::Grp1RmImm {
            kind,
            rm: Rm::Reg(dst),
            imm,
        } => JitOp::AluRI { kind, dst, imm },
        Uop::TestRmR {
            rm: Rm::Reg(a),
            reg,
        } => JitOp::TestRR { a, b: reg },
        Uop::IncR { reg } => JitOp::IncDec { reg, dec: false },
        Uop::DecR { reg } => JitOp::IncDec { reg, dec: true },
        Uop::Lea { reg, mem } => JitOp::Lea {
            dst: reg,
            base: mem.base,
            index: mem.index,
            scale: mem.scale,
            disp: mem.disp,
        },
        // ---- F1b-3: スタック形 ----
        Uop::PushR { reg } => JitOp::PushR { src: reg },
        Uop::PushImm { imm } => JitOp::PushI { imm },
        Uop::PopR { reg } => JitOp::PopR { dst: reg },
        Uop::Leave => JitOp::Leave,
        Uop::XchgAR { reg } => JitOp::XchgA { reg },
        // CAP_CHAIN: jccは終端でなく「条件つき途中退出」— 続きを同じブロックに焼く
        Uop::Jcc { cc, rel } => return Some((JitOp::Jcc { cc, rel }, caps & CAP_CHAIN == 0)),
        Uop::JmpRel { rel } => return Some((JitOp::Jmp { rel }, true)),
        Uop::CallRel { rel } => return Some((JitOp::CallRel { rel }, true)),
        Uop::Ret => return Some((JitOp::Ret, true)),
        _ => return None, // メモリ形・スタック・その他はF1a対象外
    };
    Some((op, false))
}

/// このuopがJITの語彙に入っているか (opstatsの分布計測用)
#[cfg(feature = "opstats")]
pub(crate) fn in_vocab(u: &Uop) -> bool {
    convert(u, CAP_VOCAB2).is_some()
}

/// `pa` から**走路ごと**切り出す (F1b-3のタイル焼き)。
///
/// collect_block は語彙外の命令で切れる。動的実測では語彙内が81%も
/// あるのに、語彙外18.8%が点在するせいで走路が平均5命令の断片になり、
/// 断片の頭は分岐の着地でないと熱が乗らず、カバレッジが2.3%で止まった。
/// ここでは**語彙外の1命令を飛ばして次のブロックも続けて焼く** —
/// 実行時のタイル張り (head_pending) が入口を作り、こちらが中身を量産する。
/// 熱い頭1つから走路全体が一度に焼ける (セグメントごとの再加熱1024回が消える)
pub fn collect_run(m: &Machine, pa: u32, cap: usize, max_blocks: usize) -> Vec<JitBlock> {
    collect_run_caps(m, pa, cap, max_blocks, CAP_F1B)
}

/// capsつき版 (ネイティブランナーは CAP_VOCAB2 を渡す)
pub fn collect_run_caps(
    m: &Machine,
    pa: u32,
    cap: usize,
    max_blocks: usize,
    caps: u32,
) -> Vec<JitBlock> {
    let mut blocks = Vec::new();
    let mut p = pa;
    // guard: 語彙外スキップの空回り対策 (ページ内で高々数十回)
    let mut guard = 0;
    while blocks.len() < max_blocks && guard < 64 {
        guard += 1;
        if let Some(blk) = collect_block_caps(m, p, cap, caps) {
            let end = blk
                .head_pa
                .wrapping_add(blk.ops.iter().map(|&(l, _)| l as u32).sum::<u32>());
            let terminal = matches!(
                blk.ops.last(),
                Some((
                    _,
                    JitOp::Jcc { .. } | JitOp::Jmp { .. } | JitOp::CallRel { .. } | JitOp::Ret
                ))
            );
            let hit_cap = blk.ops.len() >= cap;
            blocks.push(blk);
            if terminal {
                break; // 分岐で終わる走路 — その先は着地点の熱に任せる
            }
            p = end;
            if p & 0xFFF == 0 {
                break; // ページ末
            }
            if !hit_cap {
                // 語彙外の1命令を飛ばして続ける (長さだけ知りたい)
                match decode::decode_at(m, p) {
                    Some((len, _)) => {
                        p = p.wrapping_add(len as u32);
                        if p & 0xFFF == 0 {
                            break;
                        }
                    }
                    None => break, // uop化すらできない命令 — 走路はここまで
                }
            }
        } else {
            // 頭が語彙外: 1命令飛ばして次を試す
            match decode::decode_at(m, p) {
                Some((len, _)) => {
                    p = p.wrapping_add(len as u32);
                    if p & 0xFFF == 0 {
                        break;
                    }
                }
                None => break,
            }
        }
    }
    blocks
}

/// `pa` から直線にデコードして、JITで焼ける範囲を切り出す。
/// 対象外の命令・ページ末・`cap` で打ち切り。1命令も取れなければ None。
/// **デコードするだけ** — 機械の状態は変えない (&Machine)
pub fn collect_block(m: &Machine, pa: u32, cap: usize) -> Option<JitBlock> {
    collect_block_caps(m, pa, cap, CAP_F1B)
}

/// capsつき版
pub fn collect_block_caps(m: &Machine, pa: u32, cap: usize, caps: u32) -> Option<JitBlock> {
    let mut ops = Vec::new();
    let mut p = pa;
    while ops.len() < cap {
        let Some((len, uop)) = decode::decode_at(m, p) else {
            break; // デコード不能の手前まで
        };
        let Some((op, term)) = convert(&uop, caps) else {
            break; // JIT対象外の手前まで
        };
        ops.push((len, op));
        p = p.wrapping_add(len as u32);
        if term || p & 0xFFF == 0 {
            break; // 分岐で終端 / ページ末 (跨ぎ命令はdecode_atが拒否済み)
        }
    }
    if ops.is_empty() {
        None
    } else {
        Some(JitBlock { head_pa: pa, ops })
    }
}

/// 生成コードが直接読み書きするフィールドの**実アドレス** (wasmならリニアメモリ
/// のオフセット)。生成時に定数として焼き込む。
///
/// 有効なのは Machine が動かない間だけ — wasm側の Emulator は wasm-bindgen が
/// Box化して持つので、生成後にアドレスが変わることはない (再ロード時は
/// フィールドへの上書き代入なので番地は不変)。ネイティブで使うならBox/Pinが前提
#[derive(Debug, Clone, Copy)]
pub struct JitLayout {
    /// regs[0..8] の先頭 (u32×8、連続)
    pub regs: usize,
    pub ip: usize,
    pub flags: usize,
    pub cc_op: usize,
    pub cc_w: usize,
    pub cc_a: usize,
    pub cc_b: usize,
    pub cc_cin: usize,
    pub cc_r: usize,
    pub tsc: usize,
    pub tick_countdown: usize,
    // ---- F1c-b: TLB・変換の畳み込み用 (repr(C)が契約) ----
    /// TLBの先頭 (TlbEntry {tag:u32, base_flags:u32, leaf:u32} = 12バイト刻み)
    pub tlb: usize,
    /// TLBスロット数 (2の冪。slot = (la>>12) & (slots-1))
    pub tlb_slots: u32,
    /// ゲストRAMの先頭 (ホスト実番地) と長さ
    pub mem: usize,
    pub mem_len: usize,
    /// 隠しレジスタ配列の先頭 (SegHidden 12バイト刻み、baseはオフセット0)
    pub hidden: usize,
    /// テキストVRAM窓 [lo, hi] (書き込み高速路はこの範囲を避けてヘルパへ)
    pub vram_lo: u32,
    pub vram_hi: u32,
    /// jit_budget (このブロック実行の最大命令数) の番地 — F1c-c4
    pub jit_budget: usize,
    /// ページングが有効か見るためのCR0の番地 (bit31=PG。PG無効時は恒等変換 —
    /// 高速路はTLBを引かず la をそのまま物理に使ってよい…とはせず、
    /// **PG無効時もTLBは恒等で埋まらないので必ずヘルパへ**。生成コードは
    /// タグ不一致で自然に遅い道へ落ちる。この欄は将来用の写し
    pub cr0: usize,
}

/// TLBエントリ base_flags の下位ビット割当 (S3 — lib.rsのTlbEntryと同じ)
pub const TLB_W: u32 = 1;
pub const TLB_U: u32 = 2;
pub const TLB_D: u32 = 4;

/// 焼けたブロックの実行フック (F1a)。
///
/// coreは生成器もランタイムも知らない — 知っているのは「ブロック頭で焼けた
/// ブロックがあれば `enter(slot)` を呼べばn命令ぶん進む」という契約だけ。
/// dynではなくfnポインタなのは coreの無依存を保つため。
///
/// ブロックの有無・世代照合 (現世代か)・命令数は **Entry側**が持つ — 実行時に
/// 毎ブロック頭のhashmap照合を払わないため。フックは「スロットを呼ぶ」だけ。
/// 呼ばれるのはブロック頭 (分岐の着地直後か step_cached 入口) で、`m.cpu.ip` は
/// 先頭命令を指す。tsc/tick_countdown/extra の清算はcore側が n命令まとめて
/// 毎命令の意味順序どおりに再現する (位置がずれると命令数の決定性が壊れる)。
#[derive(Clone, Copy)]
pub struct JitHook {
    /// スロット番号を受け取り、そのJITブロックを実行して実行命令数を返す。
    /// **ブロックの全命令数より少ない返り値 = フォールト脱出** (F1b) —
    /// 返した数までは完全に実行済み、次の命令は状態を1つも変えていない。
    /// core側はその1命令をインタプリタでやり直す (skip_jit)。
    /// budget_awareなら「予算 (jit_budget) ちょうどでの途中退出」もあり得る —
    /// こちらは完全実行済みの正規出口 (やり直し不要)
    pub enter: fn(slot: u32) -> u32,
    /// 生成コードが jit_budget を毎命令照合するか (F1c-c4)。
    /// true: coreは予算1以上で入場させ、enter前に jit_budget を書く。
    /// false (wasm凍結世代): 従来どおり「ブロック全長 <= 予算」でだけ入場
    pub budget_aware: bool,
}

/// ブロックのページ世代 (焼いた時点の値を控えて、実行前に照合する)。
/// ブロック**内**で世代が動くことはない — 現行coreで世代を進めるのは
/// REP文字列と write_phys8 だけで、どちらも語彙に無い (素のストアは進めない。
/// JitOpのdocを参照)。よって頭での照合が、インタプリタの毎命令照合と同じ強さになる
pub fn page_gen(m: &Machine, pa: u32) -> u32 {
    m.dcache.page_gen_of(pa)
}

pub fn layout(m: &Machine) -> JitLayout {
    // jit.rs は cpu モジュールの子孫なので、Cpuのprivateフィールドに触れる
    // (alu.rs が cc_* に触れるのと同じ理屈)。アドレスはここで写し取り、
    // 生成器には数値としてだけ渡す
    JitLayout {
        regs: m.cpu.regs.as_ptr() as usize,
        ip: &m.cpu.ip as *const u32 as usize,
        flags: &m.cpu.flags as *const u32 as usize,
        cc_op: &m.cpu.cc_op as *const u8 as usize,
        cc_w: &m.cpu.cc_w as *const u8 as usize,
        cc_a: &m.cpu.cc_a as *const u32 as usize,
        cc_b: &m.cpu.cc_b as *const u32 as usize,
        cc_cin: &m.cpu.cc_cin as *const u32 as usize,
        cc_r: &m.cpu.cc_r as *const u32 as usize,
        tsc: &m.cpu.tsc as *const u64 as usize,
        tick_countdown: &m.tick_countdown as *const u32 as usize,
        tlb: m.tlb_base_addr(),
        tlb_slots: crate::TLB_SLOTS as u32,
        mem: m.mem.as_ptr() as usize,
        mem_len: m.mem.len(),
        hidden: m.cpu.hidden.as_ptr() as usize,
        vram_lo: crate::bus::VRAM_TEXT_BASE,
        vram_hi: crate::bus::VRAM_TEXT_END,
        jit_budget: &m.jit_budget as *const u32 as usize,
        cr0: &m.cpu.cr0 as *const u32 as usize,
    }
}
