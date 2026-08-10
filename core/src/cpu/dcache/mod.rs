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

/// ブロック表 (直接マップ) のスロット数。キーは**ブロック先頭**の物理アドレス —
/// 分岐の着地点と再開点だけなので、バイト番地ごとに1部屋要った旧方式より疎でよい
const BLOCK_SLOTS: usize = 32 * 1024;

/// 1ブロックに控えるuop数の上限。長い直線コードは複数ブロックに割れるだけ
const BLOCK_CAP: usize = 32;

/// uopプールの上限 (これを超えたら**全部捨てて**作り直す)。
/// ブロックは追記専用でプールを埋めるので、無効化や衝突で見捨てられた
/// uopは溜まる一方 — JITのコードキャッシュと同じで、たまの全フラッシュが
/// 個別の回収より安くて単純。1Muop ≒ 24MB
const POOL_MAX: usize = 1 << 20;

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

/// プールに並ぶデコード済み1命令 (B5: ブロックの中身)
#[derive(Clone, Copy)]
struct PoolUop {
    len: u8,
    uop: Uop,
}

/// ブロックの見出し。中身はプールの連続区間 [start, start+n)
///
/// **なぜブロックか (B5)**: 旧方式は命令のバイト番地ごとに32Bのエントリを
/// 直接マップで持っていた — 4KBのコードページが表の128KBに散り、
/// 毎命令のエントリloadがL1に乗らないランダムアクセスになっていた。
/// ブロックにすると、照合はブロック頭で1回、中身は**連続読み** —
/// プリフェッチが効く形になる。実CPUのuopキュー、QEMU TCGのTBと同じ答え
#[derive(Clone, Copy)]
struct BlockHead {
    /// ブロック先頭の物理アドレス (TAG_INVALID = 空き)
    tag: u32,
    /// 控えたときのページ世代。ページに書き込みがあると合わなくなる
    gen: u32,
    /// プール内の開始位置
    start: u32,
    /// uop数 (1..=BLOCK_CAP)
    n: u16,
}

pub struct DecodeCache {
    /// ブロック表 (直接マップ)。**最初の32bitデコードまで確保しない** —
    /// 16bit機やcosimの単発Machineに払わせない
    blocks: Vec<BlockHead>,
    /// デコード済みuopの置き場 (追記専用、溢れたら全フラッシュ)
    pool: Vec<PoolUop>,
    /// 物理4Kページごとの世代。書き込みで進む
    page_gen: Vec<u32>,
    /// そのページにデコード済みコードがあるか。
    /// データページへの書き込みをタダにするための1判定
    page_has_code: Vec<bool>,
    /// 観測: ヒット / 新規デコード / 対象外 (従来経路行き)
    pub hits: u64,
    pub fills: u64,
    pub fallbacks: u64,
}

impl DecodeCache {
    pub fn new(ram_bytes: usize) -> Self {
        let pages = ram_bytes.div_ceil(4096);
        DecodeCache {
            blocks: Vec::new(),
            pool: Vec::new(),
            page_gen: vec![0; pages],
            page_has_code: vec![false; pages],
            hits: 0,
            fills: 0,
            fallbacks: 0,
        }
    }

    /// 物理1バイト書き込みの通知。コードを控えたページだけ世代を進める
    #[inline]
    pub(crate) fn note_write(&mut self, pa: u32) {
        let p = (pa >> 12) as usize;
        if let Some(has) = self.page_has_code.get_mut(p) {
            if *has {
                *has = false;
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
            if let Some(has) = self.page_has_code.get_mut(p) {
                if *has {
                    *has = false;
                    self.page_gen[p] = self.page_gen[p].wrapping_add(1);
                }
            }
        }
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
        m.guard_save();
        return super::step(m);
    }
    let mut lin = m.cpu.lin(CS, m.cpu.ip);
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

    let page = (pa >> 12) as usize;
    if m.dcache.blocks.is_empty() {
        m.dcache.blocks = vec![
            BlockHead {
                tag: TAG_INVALID,
                gen: 0,
                start: 0,
                n: 0,
            };
            BLOCK_SLOTS
        ];
    }

    // 外側: ブロック単位。照合 (タグ+世代) はブロック頭で1回
    'blocks: loop {
        let gen = m.dcache.page_gen.get(page).copied().unwrap_or(0);
        let bslot = (pa as usize) & (BLOCK_SLOTS - 1);
        let h = m.dcache.blocks[bslot];
        let (start, n) = if h.tag == pa && h.gen == gen {
            m.dcache.hits += h.n as u64;
            (h.start as usize, h.n as usize)
        } else {
            match build_block(m, pa, page, gen, bslot) {
                Some(x) => x,
                None => {
                    // 先頭からデコードできない → 従来経路へ (#UD報告もそちら)
                    m.dcache.fallbacks += 1;
                    m.trap_ip = m.cpu.ip;
                    m.guard_save();
                    return super::step(m);
                }
            }
        };

        // 内側: プールの連続区間を順に実行。分岐はブロック終端にしか無いので、
        // 途中のuopは必ず直線で次へ落ちる
        for i in start..start + n {
            let PoolUop { len, uop } = m.dcache.pool[i];

            // 控えは「メモリに触るuop」だけ。キャッシュ済み命令のフェッチは
            // ページ内で完結する (跨ぎはデコード時に拒否) ので、フォールトの
            // 出どころはデータアクセスだけ — 触らないなら巻き戻しは起きない
            let touches = exec::may_touch_memory(&uop);
            if touches {
                m.guard_save_slim();
            }
            let ip_linear = m.cpu.ip.wrapping_add(len as u32);
            m.cpu.advance_ip(len as u32);
            exec::exec(m, uop);

            // ---- 連結判定: ここから先は「次の命令も続けて実行するか」 ----
            // 順序も判定も旧・毎命令方式と同一 — 割り込みの受付点・
            // フォールトの境界・時計の刻みは1命令粒度のまま動かさない
            if extra == 0 {
                return;
            }
            if m.pending_fault.get().is_some() || m.trap.is_some() || m.halted {
                return;
            }
            if m.cpu.flag(super::IF) && (m.pending_irq.is_some() || m.pic_service) {
                return;
            }
            // 次の物理番地。直線なら足すだけ、分岐でも同じ線形ページなら差し替え
            let new_lin = if m.cpu.ip == ip_linear {
                lin.wrapping_add(len as u32)
            } else {
                m.cpu.lin(CS, m.cpu.ip)
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
            // 自己書き換えの見張り: このページに書いたかもしれないuopの後だけ
            // 世代を見直す。動いていたら残りは古い — ブロックを引き直す
            // (次の照合が失敗して、新しいバイト列から組み直される)
            if touches && m.dcache.page_gen.get(page).copied().unwrap_or(0) != gen {
                continue 'blocks;
            }
        }
        // ブロックを走り切った (終端は分岐かページ末)。次のブロックへ
    }
}

/// ブロックを組む: `head_pa` から直線にデコードして、分岐 (IPを書くuop) か
/// ページ末か BLOCK_CAP で切る。1命令も取れなければ None (従来経路行き)
fn build_block(
    m: &mut Machine,
    head_pa: u32,
    page: usize,
    gen: u32,
    bslot: usize,
) -> Option<(usize, usize)> {
    // プールが一杯なら全部捨てる (見出しも道連れ)。個別回収より安くて単純
    if m.dcache.pool.len() + BLOCK_CAP > POOL_MAX {
        m.dcache.pool.clear();
        for b in &mut m.dcache.blocks {
            b.tag = TAG_INVALID;
        }
    }
    let start = m.dcache.pool.len();
    let mut pa = head_pa;
    while m.dcache.pool.len() - start < BLOCK_CAP {
        let Some((len, uop)) = decode::decode_at(m, pa) else {
            break; // デコードできない所の手前まででブロックにする
        };
        m.dcache.pool.push(PoolUop { len, uop });
        // IPを書き得るuopはブロックの終端 — これより先は着地次第
        let is_branch = matches!(
            uop,
            Uop::Jcc { .. }
                | Uop::JmpRel { .. }
                | Uop::CallRel { .. }
                | Uop::Ret
                | Uop::Grp5 { kind: 2 | 4, .. }
        );
        pa = pa.wrapping_add(len as u32);
        if is_branch || pa & 0xFFF == 0 {
            break; // 分岐 or ページ末に達した (跨ぎ命令はdecode_atが拒否済み)
        }
    }
    let n = m.dcache.pool.len() - start;
    if n == 0 {
        return None;
    }
    m.dcache.fills += n as u64;
    m.dcache.blocks[bslot] = BlockHead {
        tag: head_pa,
        gen,
        start: start as u32,
        n: n as u16,
    };
    if let Some(has) = m.dcache.page_has_code.get_mut(page) {
        *has = true;
    }
    Some((start, n))
}
