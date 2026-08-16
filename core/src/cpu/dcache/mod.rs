//! デコード済み命令キャッシュ (ADR-0007 の本丸、P1a)。
//!
//! 同じ命令を何百万回もデコードし直すのをやめる。物理アドレスをキーに
//! デコード結果 (Uop) を控え、2回目からはデコードを飛ばして実行だけを行う。
//! 実CPUのuopキャッシュ、QEMU TCG のTBと同じ系譜の答えである。
//!
//! ## 対象は実測の上位だけ (opstats で選定)
//!
//! Linuxブート624M命令の実測で、mov (89/8B) 24% / ALUグリッド ~16% /
//! jcc ~10% / lea 4.7% / シフト 4.5% / movzx 3.8% / test 3.5% が上位。
//! これらを Uop 化し、**それ以外は従来の `cpu::step` にそのまま落とす**。
//! 全命令を一気にIR化しない — フォールバックがあるから安全に刻める。
//!
//! ## 意味を変えないための約束
//!
//! - 実行器は従来経路と**同じヘルパ** (alu8/alu_w/shift_rot/condition/
//!   push_w/pop_w) を呼ぶ。意味論を二重実装しない
//! - 0x66/0x67/REP 付きは対象外 (従来経路へ)。prefixed_ops の観測も保たれる
//! - ページを跨ぐ命令は控えない — 無効化の世代がページ単位のため
//! - デバッガONのときは使わない (before_exec/トレースの意味を守る)
//! - 16bitコードは対象外 (ELKS/FreeDOSは従来経路のまま)
//!
//! ## 自己書き換えは書き込みで受ける
//!
//! DOSどころかLinuxも起動時にコードを書き換える (alternatives/jump label)。
//! TLB・VRAM検出と同じ発想で、**コードを控えたページへの書き込み**が
//! そのページの世代を進め、古い控えは照合で外れる。データページへの
//! 書き込みは has_code の1判定だけで素通りする。

use super::CS;
use crate::Machine;

// 部屋割り: デコード (バイト列→Uop) と実行 (Uop→従来ヘルパ) は別ファイル。
// B3 (ペア融合) や B4 (ブロック連結) が来てもここに部屋を足すだけで済む
mod decode;
mod exec;

/// 直接マップのスロット数。ブートの熱い命令アドレス集合を覆う広さと、
/// ホストのキャッシュに収まる小ささの折り合い (768KB)。要調整なら実測で
const SLOTS: usize = 128 * 1024;

const TAG_INVALID: u32 = 0xFFFF_FFFF;

/// メモリオペランドの形。**解決済みの番地ではなく作り方**を持つ —
/// 実効アドレスはレジスタの今の値から実行のたびに組む
#[derive(Clone, Copy)]
pub(crate) struct MemRef {
    /// 基底レジスタ (-1 = 無し)
    base: i8,
    /// インデックスレジスタ (-1 = 無し)
    index: i8,
    scale: u8,
    /// セグメント (デコード時に上書き規則まで解決済み)
    seg: u8,
    disp: u32,
}

#[derive(Clone, Copy)]
pub(crate) enum Rm {
    Reg(u8),
    Mem(MemRef),
}

/// デコード済み命令。従来経路の各armと1対1で対応する
#[derive(Clone, Copy)]
pub(crate) enum Uop {
    /// 89: mov r/m32, r32
    MovRmR {
        rm: Rm,
        reg: u8,
    },
    /// 8B: mov r32, r/m32
    MovRRm {
        reg: u8,
        rm: Rm,
    },
    /// 88 / 8A (8bit)
    Mov8RmR {
        rm: Rm,
        reg: u8,
    },
    Mov8RRm {
        reg: u8,
        rm: Rm,
    },
    /// 66 89: mov rm16, r16 (上位16bitは不変)
    Mov16RmR {
        rm: Rm,
        reg: u8,
    },
    /// 66 8B: mov r16, rm16 (上位16bitは不変)
    Mov16RRm {
        reg: u8,
        rm: Rm,
    },
    /// B8-BF: mov r32, imm32
    MovRImm {
        reg: u8,
        imm: u32,
    },
    /// ALUグリッド op&7==1 (01/09/…/39): kind = (op>>3)&7
    AluRmR {
        kind: u8,
        rm: Rm,
        reg: u8,
    },
    /// op&7==3 (03/0B/…/3B)
    AluRRm {
        kind: u8,
        reg: u8,
        rm: Rm,
    },
    /// op&7==0 / 2 (8bit)
    Alu8RmR {
        kind: u8,
        rm: Rm,
        reg: u8,
    },
    Alu8RRm {
        kind: u8,
        reg: u8,
        rm: Rm,
    },
    /// op&7==5: eAX, imm
    AluAImm {
        kind: u8,
        imm: u32,
    },
    /// op&7==4: AL, imm8
    Alu8AImm {
        kind: u8,
        imm: u8,
    },
    /// 81/83: GRP1 r/m32, imm (0x83の符号拡張はデコード時に済ませてある)
    Grp1RmImm {
        kind: u8,
        rm: Rm,
        imm: u32,
    },
    /// 80: GRP1 r/m8, imm8
    Grp18RmImm {
        kind: u8,
        rm: Rm,
        imm: u8,
    },
    /// 85 / 84: test
    TestRmR {
        rm: Rm,
        reg: u8,
    },
    Test8RmR {
        rm: Rm,
        reg: u8,
    },
    /// 8D: lea (セグメントを適用しない実効オフセット)
    Lea {
        reg: u8,
        mem: MemRef,
    },
    /// 70-7F: jcc rel8 / 0F 80-8F: jcc rel32 (relは拡張済み)
    Jcc {
        cc: u8,
        rel: u32,
    },
    /// E9 / EB
    JmpRel {
        rel: u32,
    },
    /// E8
    CallRel {
        rel: u32,
    },
    /// C3
    Ret,
    /// 50-57 / 58-5F
    PushR {
        reg: u8,
    },
    PopR {
        reg: u8,
    },
    /// C1: shift r/m32, imm8 / D3: shift r/m32, CL (kindはModRMのreg欄)
    ShiftRmImm {
        kind: u8,
        rm: Rm,
        count: u8,
    },
    ShiftRmCl {
        kind: u8,
        rm: Rm,
    },
    /// 0F B6: movzx r32, r/m8
    MovzxB {
        reg: u8,
        rm: Rm,
    },
    // ---- ここから P1b (フォールバック43Mの実測上位から追加) ----
    /// 40-47 / 48-4F: inc/dec r32 (CFを触らないのがADD/SUBとの違い)
    IncR {
        reg: u8,
    },
    DecR {
        reg: u8,
    },
    /// C7 /any: mov r/m32, imm32 (regは無視 — 従来経路と同じ) / C6: 8bit
    MovRmImm {
        rm: Rm,
        imm: u32,
    },
    MovRm8Imm {
        rm: Rm,
        imm: u8,
    },
    /// A1 / A3: mov eax↔moffs32 (segは解決済み)
    MovAMoffs {
        load: bool,
        seg: u8,
        off: u32,
    },
    /// A0 / A2 (8bit)
    Mov8AMoffs {
        load: bool,
        seg: u8,
        off: u32,
    },
    /// 68 / 6A: push imm (6Aの符号拡張はデコード時に済み)
    PushImm {
        imm: u32,
    },
    /// C9: leave
    Leave,
    /// 90-97: xchg eAX, r (90はnop = 自分と交換)
    XchgAR {
        reg: u8,
    },
    /// F6 / F7 の kind 0-3 (test imm / not / neg)。mul/divは従来経路
    Grp3b {
        kind: u8,
        rm: Rm,
        imm: u8,
    },
    Grp3w {
        kind: u8,
        rm: Rm,
        imm: u32,
    },
    /// FF: inc/dec r/m (0/1)、call間接 (2)、jmp間接 (4)、push r/m (6)
    Grp5 {
        kind: u8,
        rm: Rm,
    },
    /// 0F 90-9F: setcc r/m8
    SetCC {
        cc: u8,
        rm: Rm,
    },
    /// 0F B7: movzx r32, r/m16
    MovzxW {
        reg: u8,
        rm: Rm,
    },
    /// 0F AF: imul r32, r/m32
    ImulRRm {
        reg: u8,
        rm: Rm,
    },
    /// A4-AF (REPなしの単発ストリング命令)。意味論は従来の string::exec に委譲
    StrOne {
        op: u8,
        seg: i8,
    },
}

/// len_flags のビット割当: 下位4bit = 命令長 (x86最大15B)、
/// bit4 = メモリに触るuop (guard判定のmatch再実行を消す)、
/// bit5 = 制御uop (実行後の ip==直線 比較を省ける)。
/// デコード時に一度だけ分類し、実行時はビット1つを見る —
/// 「デコード済みなのに毎命令再分類していた」を消す (2026-08-13)
const LEN_MASK: u8 = 0x0F;
const F_MEM: u8 = 0x10;
const F_CTL: u8 = 0x20;

#[derive(Clone, Copy)]
struct Entry {
    /// 命令先頭の物理アドレス (TAG_INVALID = 空き)
    tag: u32,
    /// 控えたときのページ世代。ページに書き込みがあると合わなくなる
    gen: u32,
    /// 命令長 + 属性ビット (LEN_MASK/F_MEM/F_CTL)
    len_flags: u8,
    uop: Uop,
}

pub struct DecodeCache {
    /// 直接マップ。**最初の32bitデコードまで確保しない** —
    /// 16bit機やcosimの単発Machineに768KBずつ払わせない
    entries: Vec<Entry>,
    /// 物理4Kページごとの世代。書き込みで進む
    page_gen: Vec<u32>,
    /// そのページにデコード済みコードがあるか (1ページ1bit)。
    /// データページへの書き込みをタダにするための1判定。
    /// Vec<bool> (1ページ1バイト = 128MBで32KB) だと全ストアが引く配列が
    /// L1に収まりきらない。1bit詰め (4KB) ならL1に居座る — note_writeを
    /// 全ストア経路に配線した (ADR-0020 P0) ときの税をここで消す
    page_has_code: Vec<u64>,
    /// 観測: ヒット / 新規デコード / 対象外 (従来経路行き)
    pub hits: u64,
    pub fills: u64,
    pub fallbacks: u64,
    /// 従来経路落ちの理由 (opstats時のみ計上)。語彙拡大の的はこれで決める:
    /// [0]=0x66 [1]=0x67 [2]=REP(F2/F3) [3]=LOCK [4]=ページ端(跨ぎ疑い)
    /// [5]=語彙外オペコード [6]=16bitモード (CS.D=0、fallbacksとは別口)
    pub fb_reasons: [u64; 7],
}

impl DecodeCache {
    pub fn new(ram_bytes: usize) -> Self {
        let pages = ram_bytes.div_ceil(4096);
        DecodeCache {
            entries: Vec::new(),
            page_gen: vec![0; pages],
            page_has_code: vec![0; pages.div_ceil(64)],
            hits: 0,
            fills: 0,
            fallbacks: 0,
            fb_reasons: [0; 7],
        }
    }

    /// 物理1バイト書き込みの通知。コードを控えたページだけ世代を進める。
    /// ビットが立っていない (= コード無しページ) なら分岐1つで抜ける —
    /// ここが全ストアの通り道なので、この形より重くしない
    #[inline]
    pub(crate) fn note_write(&mut self, pa: u32) {
        let p = (pa >> 12) as usize;
        if let Some(w) = self.page_has_code.get_mut(p >> 6) {
            let bit = 1u64 << (p & 63);
            if *w & bit != 0 {
                *w &= !bit;
                self.page_gen[p] = self.page_gen[p].wrapping_add(1);
            }
        }
    }

    /// 範囲書き込みの通知 (REP一括処理など、write_phys8を通らない道)
    pub(crate) fn note_write_range(&mut self, pa: u32, len: usize) {
        if len == 0 {
            return;
        }
        let first = (pa >> 12) as usize;
        let last = ((pa as usize).saturating_add(len - 1)) >> 12;
        for p in first..=last {
            self.note_write((p as u32) << 12);
        }
    }
}

/// uopの粗い名前 (opstatsの分布表示用)。メモリ形かどうかで分ける
#[cfg(feature = "opstats")]
fn uop_name(u: &Uop) -> &'static str {
    let mem = |rm: &Rm| matches!(rm, Rm::Mem(_));
    match u {
        Uop::MovRmR { rm, .. } => {
            if mem(rm) {
                "mov [m],r (対象内のはず)"
            } else {
                "mov r,r"
            }
        }
        Uop::Mov8RmR { rm, .. } => {
            if mem(rm) {
                "mov8 [m],r"
            } else {
                "mov8 r,r"
            }
        }
        Uop::Mov8RRm { rm, .. } => {
            if mem(rm) {
                "mov8 r,[m]"
            } else {
                "mov8 r,r"
            }
        }
        Uop::Alu8RmR { .. } | Uop::Alu8RRm { .. } | Uop::Alu8AImm { .. } => "alu8",
        Uop::Grp18RmImm { .. } => "grp1-8",
        Uop::Test8RmR { .. } => "test8",
        Uop::MovRm8Imm { .. } => "mov8 [m],imm",
        Uop::Mov8AMoffs { .. } => "mov8 moffs",
        Uop::MovAMoffs { .. } => "mov moffs",
        Uop::ShiftRmImm { rm, .. } | Uop::ShiftRmCl { rm, .. } => {
            if mem(rm) {
                "shift [m]"
            } else {
                "shift r"
            }
        }
        Uop::MovzxB { rm, .. } => {
            if mem(rm) {
                "movzx8 r,[m]"
            } else {
                "movzx8 r,r"
            }
        }
        Uop::MovzxW { rm, .. } => {
            if mem(rm) {
                "movzx16 r,[m]"
            } else {
                "movzx16 r,r"
            }
        }
        Uop::Grp3b { .. } => "grp3-8",
        Uop::Grp3w { .. } => "grp3",
        Uop::Grp5 { .. } => "grp5 (inc/call/jmp/push rm)",
        Uop::SetCC { .. } => "setcc",
        Uop::ImulRRm { .. } => "imul",
        Uop::StrOne { .. } => "string単発",
        Uop::CallRel { .. } => "call rel (対象内のはず)",
        Uop::Ret => "ret (対象内のはず)",
        Uop::Leave => "leave (対象内のはず)",
        _ => "その他 (対象内のはず)",
    }
}

/// 従来経路落ちの理由分類 (opstats専用の診断 — ホットパス外)。
/// decode_atの却下条件を軽く写す: プレフィクス→ページ端→語彙外の順
#[allow(dead_code)]
fn classify_fallback(m: &mut Machine, pa: u32) {
    let start = pa as usize;
    if start >= m.mem.len() {
        return;
    }
    let end = ((start | 0xFFF) + 1).min(m.mem.len());
    let b = &m.mem[start..end];
    let mut i = 0usize;
    loop {
        match b.get(i) {
            Some(0x66) => {
                m.dcache.fb_reasons[0] += 1;
                return;
            }
            Some(0x67) => {
                m.dcache.fb_reasons[1] += 1;
                return;
            }
            Some(0xF2) | Some(0xF3) => {
                m.dcache.fb_reasons[2] += 1;
                return;
            }
            Some(0xF0) => {
                m.dcache.fb_reasons[3] += 1;
                return;
            }
            Some(0x26 | 0x2E | 0x36 | 0x3E | 0x64 | 0x65) => i += 1,
            Some(_) => break,
            None => {
                m.dcache.fb_reasons[4] += 1; // ページ端でプレフィクスすら読めない
                return;
            }
        }
    }
    // 命令窓がページ端で切れている疑い (decode_atは16B窓で見る)
    if end - start < 16 {
        m.dcache.fb_reasons[4] += 1;
    } else {
        m.dcache.fb_reasons[5] += 1;
    }
}

/// キャッシュ経由の実行。対象外は従来の [`super::step`] へ落ちる。
///
/// ## ブロック連結 (B4)
///
/// 1命令実行して外側の帳簿へ戻る代わりに、`chain_extra` 命令ぶんまで
/// **この中で連結して実行し続ける**。次の命令が同じ物理ページに居るかぎり、
/// lin計算・ページ変換・外側のはしご (デバッガ判定・HLT判定・BIOS入口判定…)
/// を払い直さずに、スロット照合だけで次のuopへ直行する。
///
/// 意味を変えないための約束:
/// - **時計は1命令粒度のまま**。連結中も毎命令 tsc+=1 と装置tickを、外側の
///   [`Machine::step_inner`] と同じ順序で回す。命令数の決定性は崩れない
/// - **割り込みの受付点も1命令粒度のまま**。毎命令の境界で保留を見て、
///   受けられるなら連結を打ち切って外へ返す (配送は外側の次の呼び出し —
///   従来と同じ境界で同じ配送になる)
/// - **タグ+世代の照合は毎命令やる**。連結は照合を飛ばさない — 自己書き換えの
///   検出は非連結時と同一。連結が省くのは変換とはしごだけ
/// - 分岐で途切れない: IPが直線から外れても、行き先が同じ線形ページなら
///   物理番地を差し替えて続行する (ループがまるごと連結される)。
///   ページ表の書き換えは実機同様TLB/invlpgの約束の上に居るし、
///   mov cr3 / invlpg はuopに無いので連結はそこで自然に切れる
/// - TF (シングルステップ) 中は連結しない — 毎命令 INT1 の意味を守る
pub(crate) fn step_cached(m: &mut Machine, chain_extra: u64) {
    // 16bitコードは対象外 (ELKS/FreeDOSは従来経路)
    if !m.cpu.seg_is32(CS) {
        if cfg!(feature = "opstats") {
            m.dcache.fb_reasons[6] += 1;
        }
        m.guard_save();
        return super::step(m);
    }
    // CS基底はブロック中不変 (CSを変える命令は語彙外) — 掴み置きして
    // 分岐着地のlin()呼び (seg_base読み+pe判定) を足し算にする
    let cs_base = m.cpu.lin(CS, 0);
    let mut lin = cs_base.wrapping_add(m.cpu.ip);
    let Ok(mut pa) = m.translate_for(lin, false) else {
        // フェッチがフォールトする状況は従来経路に任せる (#PF配送もそちら)
        m.guard_save();
        return super::step(m);
    };
    let mut extra = if m.cpu.flag(super::TF) {
        0
    } else {
        chain_extra
    };
    loop {
        // 前の命令のページウォークが立てた A/D を表へ反映。
        // 空なら真偽値1つ — 熱い経路に足してよいのはこのサイズまで (B5/C5の教訓)
        m.flush_ad();
        let page = (pa >> 12) as usize;
        let slot = (pa as usize) & (SLOTS - 1);

        // Entry照合 (gen一致 = 「この命令は現世代」の保証)
        let mut cached = None;
        if !m.dcache.entries.is_empty() {
            let gen_now = m.dcache.page_gen.get(page).copied().unwrap_or(0);
            let e = &m.dcache.entries[slot];
            if e.tag == pa && e.gen == gen_now {
                cached = Some((e.len_flags, e.uop));
                // ヒットカウンタは毎命令の同番地storeになる — 計測時だけ数える
                if cfg!(feature = "opstats") {
                    m.dcache.hits += 1;
                }
            }
        }

        let (lf, uop) = match cached {
            Some(x) => x,
            None => match decode::decode_at(m, pa) {
                Some((len, uop)) => {
                    if m.dcache.entries.is_empty() {
                        m.dcache.entries = vec![
                            Entry {
                                tag: TAG_INVALID,
                                gen: 0,
                                len_flags: 0,
                                uop: Uop::Ret,
                            };
                            SLOTS
                        ];
                    }
                    let gen = m.dcache.page_gen.get(page).copied().unwrap_or(0);
                    // 分類はデコード時の一度だけ — 実行時はビットを見るだけ
                    let mut lf = len;
                    debug_assert!(len & !LEN_MASK == 0);
                    if exec::may_touch_memory(&uop) {
                        lf |= F_MEM;
                    }
                    if exec::is_control(&uop) {
                        lf |= F_CTL;
                    }
                    m.dcache.entries[slot] = Entry {
                        tag: pa,
                        gen,
                        len_flags: lf,
                        uop,
                    };
                    if let Some(w) = m.dcache.page_has_code.get_mut(page >> 6) {
                        *w |= 1 << (page & 63);
                    }
                    m.dcache.fills += 1;
                    (lf, uop)
                }
                None => {
                    m.dcache.fallbacks += 1;
                    if cfg!(feature = "opstats") {
                        classify_fallback(m, pa);
                    }
                    // 連結中に来たら trap_ip を今の現場に直す (外側で控えたのは
                    // ブロック先頭のIP)
                    m.trap_ip = m.cpu.ip;
                    m.guard_save();
                    return super::step(m);
                }
            },
        };

        // 控えは「メモリに触るuop」だけ。キャッシュ済み命令のフェッチは
        // ページ内で完結する (跨ぎはデコード時に拒否) ので、フォールトの
        // 出どころはデータアクセスだけ — 触らないなら巻き戻しは起きない。
        // 判定はデコード時に焼いたF_MEMビット (uopのmatch再実行をしない)
        let len = (lf & LEN_MASK) as u32;
        if lf & F_MEM != 0 {
            m.guard_save_slim();
        }
        let prev_ip = m.cpu.ip; // 巻き戻し先 (exec内で控えるarm用)
        m.cpu.advance_ip32(len);
        let ip_linear = m.cpu.ip; // 直線ならexec後もこのまま
        exec::exec(m, uop, prev_ip);

        // ---- 連結判定: ここから先は「次の命令も続けて実行するか」 ----
        if extra == 0 {
            return;
        }
        // フォールトの巻き戻しと#UDの裁きは外側の担当。割り込みが受けられる
        // 状態になったら境界で外へ返す (配送点は非連結時と同じ)
        if m.pending_fault.get().is_some() || m.trap.is_some() || m.halted {
            return;
        }
        if m.cpu.flag(super::IF) && (m.pending_irq.is_some() || m.pic_service) {
            return;
        }
        // 次の物理番地。直線なら足すだけ、分岐でも同じ線形ページなら差し替え
        let new_lin = if lf & F_CTL == 0 {
            // 非制御uopはIPに触らない — 比較すら不要で直線が確定
            lin.wrapping_add(len)
        } else if m.cpu.ip == ip_linear {
            lin.wrapping_add(len)
        } else {
            cs_base.wrapping_add(m.cpu.ip)
        };
        if new_lin >> 12 != lin >> 12 {
            return; // ページを跨いだら外へ (変換からやり直し)
        }
        pa = (pa & !0xFFF) | (new_lin & 0xFFF);
        lin = new_lin;
        // 帳簿: 時計と装置を外側と同じ順で1命令ぶん進める
        extra -= 1;
        m.cpu.tsc = m.cpu.tsc.wrapping_add(1);
        m.tick_countdown -= 1;
        if m.tick_countdown == 0 {
            m.tick_countdown = crate::INSTRUCTIONS_PER_TICK;
            m.tick_devices(1);
        }
    }
}
