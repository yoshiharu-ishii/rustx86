//! virtio (legacy 0.9.5) — **準仮想化装置の共通の口**。
//!
//! NE2000やATAは「実在のチップの顔」をゲストに見せるが、virtioは逆で、
//! **相手がエミュレータだとゲストが知っている**前提の規格である。だから
//! 転送の単位が「レジスタを叩く」ではなく「メモリに置いた記述子を渡す」になる —
//! ゲストが物理アドレスを書いた表を用意し、ホストはそれを読んでmemcpyする。
//! 1セクタをI/Oトラップ256回で運ぶATA PIOと違い、**通知1回で何要求でも運べる**。
//!
//! ## virtqueue (split ring) の3枚の表
//!
//! ゲストが1ページ境界に組む。番地はゲスト物理:
//!
//! ```text
//!   desc[N]   {addr u64, len u32, flags u16, next u16} ×N   買い物袋の中身
//!   avail     {flags u16, idx u16, ring[N] u16}             「袋を置いたよ」(ゲスト→ホスト)
//!   used      {flags u16, idx u16, ring[N] {id,len} u32×2}  「済んだよ」(ホスト→ゲスト)
//! ```
//!
//! descはNEXTフラグで鎖になる。avail.idxは**増える一方**で、リングの位置は
//! `idx % N` — 一周しても番号は戻らない (戻すと「新しい袋か既読か」が
//! 区別できない)。usedも同じ作法でホストが増やす。
//!
//! ## legacyを選んだ理由
//!
//! modern (virtio 1.0) はPCI capabilityでMMIO窓を配る立て付けで、面が広い。
//! legacyはBAR0のI/Oポート24バイト+設定で完結し、LinuxのドライバはどちらでもOK
//! (`virtio_pci_legacy_dev.ko`)。**同じ結果に対して狭い面から入る。**
//!
//! ここは素子 — リングの機構とレジスタ窓だけを持ち、要求のバイト列が
//! 何を意味するか (ブロックの読み書き等) は基板 ([`card`](crate::dev::card)) が持つ。

/// legacyレジスタ窓のオフセット (BAR0からのずれ)
pub mod reg {
    /// ホストが名乗る機能ビット (R, u32)
    pub const HOST_FEATURES: u16 = 0x00;
    /// ゲストが「これを使う」と返す機能ビット (W, u32)
    pub const GUEST_FEATURES: u16 = 0x04;
    /// 選択中のキューのページ番号 (RW, u32)。**PFN = 物理アドレス >> 12**。
    /// 0を書くとキューを畳む
    pub const QUEUE_PFN: u16 = 0x08;
    /// 選択中のキューの段数 (R, u16)。ホストが決め、ゲストは読むだけ
    pub const QUEUE_NUM: u16 = 0x0C;
    /// どのキューを見るか (W, u16)。ブロック装置はキュー0しか持たない
    pub const QUEUE_SEL: u16 = 0x0E;
    /// 「袋を置いたよ」の呼び鈴 (W, u16)。値は鳴らしたキューの番号
    pub const QUEUE_NOTIFY: u16 = 0x10;
    /// 装置の状態 (RW, u8)。ACKNOWLEDGE→DRIVER→DRIVER_OKと立っていく。
    /// **0を書かれたらリセット** — 全部を組み立て前に戻す
    pub const STATUS: u16 = 0x12;
    /// 割り込みの理由 (R, u8)。**読むと0に戻る** — 読んだことがACKである
    pub const ISR: u16 = 0x13;
    /// ここから装置固有の設定 (ブロックなら容量など)
    pub const CONFIG: u16 = 0x14;
}

/// desc.flags: 鎖が続く
const DESC_NEXT: u16 = 1;
/// desc.flags: この袋は**装置が書く**側 (無ければ装置は読むだけ)
const DESC_WRITE: u16 = 2;
/// avail.flags: 「済んでも割り込みは要らない」(ゲストがポーリング中)
const AVAIL_NO_INTERRUPT: u16 = 1;

/// 鎖の1節。ゲスト物理アドレスと長さ、装置がどちら向きに触るか
#[derive(Debug, Clone, Copy)]
pub struct Seg {
    pub addr: u32,
    pub len: u32,
    /// trueなら装置→ゲスト (読みの結果を置く側)。falseなら装置は読むだけ
    pub write: bool,
}

/// legacy virtio-PCI の素子 — レジスタ窓とキュー1本。
///
/// キューを1本しか持たないのは、ブロックもネットの片方向も1本で足りるから
/// (virtio-netは受信/送信で2本要る。要る装置が来たら配列にする —
/// 無い装置のために器だけ先に作らない)。
#[derive(Debug, Clone)]
pub struct VirtioPci {
    /// 装置の状態 (STATUS)。ゲストが立てていく
    pub status: u8,
    /// 割り込みの理由。bit0 = キューが進んだ。読むと0に戻る
    isr: u8,
    /// ホストが名乗る機能。ブロックは0で足りる (追加機能なしの素の姿)
    pub host_features: u32,
    /// ゲストが選んだ機能
    pub guest_features: u32,
    /// キュー0のPFN (物理 >> 12)。0なら未設定
    pub queue_pfn: u32,
    /// キューの段数。**2の冪** (リングの位置を % で出すため)
    qsize: u16,
    /// QUEUE_SELの現在値。0以外を選ばれたらPFN/NUMは0を答える
    queue_sel: u16,
    /// availをどこまで読んだか (増える一方の側の写し)
    last_avail: u16,
    /// usedをどこまで書いたか
    used_idx: u16,
    /// 呼び鈴が鳴った (次のtickで処理する)
    notified: bool,
    /// 装置固有の設定 (CONFIG以降で読める)。基板が組み立て時に置く
    config: Vec<u8>,
    /// 32bitレジスタをバイトで書かれるときの組み立て場所。
    /// CPU側のOUTは8bitに分解されて届くので、**4回で1語**を貯める
    wbuf: [u8; 4],
}

impl VirtioPci {
    pub fn new(qsize: u16, config: Vec<u8>) -> Self {
        debug_assert!(qsize.is_power_of_two());
        Self {
            status: 0,
            isr: 0,
            host_features: 0,
            guest_features: 0,
            queue_pfn: 0,
            qsize,
            queue_sel: 0,
            last_avail: 0,
            used_idx: 0,
            notified: false,
            config,
            wbuf: [0; 4],
        }
    }

    /// レジスタ窓の読み (BAR0からのずれ)。**多バイトのレジスタもバイトで読まれる**
    /// (CPUのINは8bitに分解されて届く) ので、値のバイト位置で答える
    pub fn read(&mut self, off: u16) -> u8 {
        let b = |v: u32, at: u16| (v >> (8 * at)) as u8;
        match off {
            reg::HOST_FEATURES..=0x03 => b(self.host_features, off),
            reg::GUEST_FEATURES..=0x07 => b(self.guest_features, off - reg::GUEST_FEATURES),
            reg::QUEUE_PFN..=0x0B => {
                // 選択が範囲外なら「そんなキューは無い」= 0
                let v = if self.queue_sel == 0 {
                    self.queue_pfn
                } else {
                    0
                };
                b(v, off - reg::QUEUE_PFN)
            }
            reg::QUEUE_NUM..=0x0D => {
                let v = if self.queue_sel == 0 {
                    u32::from(self.qsize)
                } else {
                    0
                };
                b(v, off - reg::QUEUE_NUM)
            }
            reg::QUEUE_SEL | 0x0F => b(u32::from(self.queue_sel), off - reg::QUEUE_SEL),
            reg::QUEUE_NOTIFY | 0x11 => 0,
            reg::STATUS => self.status,
            reg::ISR => {
                // 読んだことがACK。**ここで下ろす**からレベルではなくエッジで足りる
                std::mem::replace(&mut self.isr, 0)
            }
            _ => *self
                .config
                .get(usize::from(off - reg::CONFIG))
                .unwrap_or(&0xFF),
        }
    }

    /// レジスタ窓の書き。u32のレジスタは4バイトで貯めて、**最上位が来たら確定**
    /// (LinuxのiowriteはLE順で下位から書くので、上位バイトが締めになる)
    pub fn write(&mut self, off: u16, val: u8) {
        match off {
            reg::GUEST_FEATURES..=0x07 => {
                let at = usize::from(off - reg::GUEST_FEATURES);
                self.wbuf[at] = val;
                if at == 3 {
                    self.guest_features = u32::from_le_bytes(self.wbuf);
                }
            }
            reg::QUEUE_PFN..=0x0B => {
                let at = usize::from(off - reg::QUEUE_PFN);
                self.wbuf[at] = val;
                if at == 3 && self.queue_sel == 0 {
                    self.queue_pfn = u32::from_le_bytes(self.wbuf);
                    // キューを組み直したら読み位置も最初から
                    self.last_avail = 0;
                    self.used_idx = 0;
                }
            }
            reg::QUEUE_SEL => self.queue_sel = (self.queue_sel & 0xFF00) | u16::from(val),
            0x0F => self.queue_sel = (self.queue_sel & 0x00FF) | (u16::from(val) << 8),
            // 呼び鈴。キューは1本なので**下位バイトが来た時点で鳴ったと分かる**
            // (値=キュー番号は0しか来ない。上位バイトは黙って受ける)
            reg::QUEUE_NOTIFY => self.notified = true,
            0x11 => {}
            reg::STATUS => {
                self.status = val;
                if val == 0 {
                    // リセット — ドライバの組み立て前に戻す
                    self.isr = 0;
                    self.guest_features = 0;
                    self.queue_pfn = 0;
                    self.queue_sel = 0;
                    self.last_avail = 0;
                    self.used_idx = 0;
                    self.notified = false;
                }
            }
            _ => {} // HOST_FEATURES / QUEUE_NUM / ISR / CONFIG は読み専用
        }
    }

    /// 呼び鈴が鳴っていたら回収する (読むと下りる)
    pub fn take_notify(&mut self) -> bool {
        std::mem::replace(&mut self.notified, false)
    }

    /// キューが使える状態か (PFNが配られ、ドライバがOKを出している)
    pub fn queue_ready(&self) -> bool {
        self.queue_pfn != 0
    }

    // --- リングの番地 (legacyの決まった並び) ---

    fn desc_base(&self) -> u64 {
        u64::from(self.queue_pfn) << 12
    }
    fn avail_base(&self) -> u64 {
        self.desc_base() + u64::from(self.qsize) * 16
    }
    fn used_base(&self) -> u64 {
        // avail表の後ろを**次のページ境界へ切り上げた所**から。regではなく
        // 仕様の決まり (legacyはこの1点で表の位置が全部決まる)
        let end = self.avail_base() + 6 + u64::from(self.qsize) * 2;
        (end + 4095) & !4095
    }

    /// availに未読の袋があれば1つ取り出す。返すのは (先頭desc番号, 鎖の中身)。
    /// RAMの外を指す表は**その場で畳む** (壊れた表を歩き続けない)
    pub fn pop_avail(&mut self, ram: &[u8]) -> Option<(u16, Vec<Seg>)> {
        if !self.queue_ready() {
            return None;
        }
        let avail_idx = r16(ram, self.avail_base() + 2)?;
        if self.last_avail == avail_idx {
            return None; // 新しい袋は無い
        }
        let slot = u64::from(self.last_avail % self.qsize);
        let head = r16(ram, self.avail_base() + 4 + slot * 2)?;
        self.last_avail = self.last_avail.wrapping_add(1);

        // 鎖を歩く。**上限はqsize** — nextが輪になっていても止まれる
        let mut segs = Vec::new();
        let mut i = head;
        for _ in 0..self.qsize {
            let d = self.desc_base() + u64::from(i % self.qsize) * 16;
            let addr = r32(ram, d)?; // 上位32bitは読まない (32bit機のRAMに上位は無い)
            let len = r32(ram, d + 8)?;
            let flags = r16(ram, d + 12)?;
            segs.push(Seg {
                addr,
                len,
                write: flags & DESC_WRITE != 0,
            });
            if flags & DESC_NEXT == 0 {
                return Some((head, segs));
            }
            i = r16(ram, d + 14)?;
        }
        None // 鎖が輪 — 袋ごと捨てる (仕様上ドライバのバグ)
    }

    /// 「済んだよ」をusedに書く。`written` は装置がゲストRAMへ書いたバイト数。
    /// 戻り値は**割り込みを上げるべきか** (ゲストが「要らない」と言っていなければtrue)
    #[must_use]
    pub fn push_used(&mut self, ram: &mut [u8], head: u16, written: u32) -> bool {
        let slot = u64::from(self.used_idx % self.qsize);
        let at = self.used_base() + 4 + slot * 8;
        w32(ram, at, u32::from(head));
        w32(ram, at + 4, written);
        self.used_idx = self.used_idx.wrapping_add(1);
        // idxは**中身を書き終えてから**進める。逆だとゲストが書きかけを読む
        w16(ram, self.used_base() + 2, self.used_idx);
        self.isr |= 1;
        let flags = r16(ram, self.avail_base()).unwrap_or(0);
        flags & AVAIL_NO_INTERRUPT == 0
    }

    /// 割り込みを上げるべきか (ISRが立ったままか)
    pub fn irq_pending(&self) -> bool {
        self.isr != 0
    }

    pub fn save(&self, w: &mut crate::snapshot::Writer) {
        w.u8(self.status);
        w.u8(self.isr);
        w.u32(self.host_features);
        w.u32(self.guest_features);
        w.u32(self.queue_pfn);
        w.u16(self.qsize);
        w.u16(self.queue_sel);
        w.u16(self.last_avail);
        w.u16(self.used_idx);
        w.bool(self.notified);
        w.bytes(&self.config);
    }

    pub fn load(r: &mut crate::snapshot::Reader) -> Result<Self, String> {
        let mut v = Self::new(2, Vec::new()); // 仮の器 (直後に全欄を上書き)
        v.status = r.u8()?;
        v.isr = r.u8()?;
        v.host_features = r.u32()?;
        v.guest_features = r.u32()?;
        v.queue_pfn = r.u32()?;
        v.qsize = r.u16()?;
        v.queue_sel = r.u16()?;
        v.last_avail = r.u16()?;
        v.used_idx = r.u16()?;
        v.notified = r.bool()?;
        v.config = r.bytes()?;
        if !v.qsize.is_power_of_two() {
            return Err(format!("virtioのキュー段数が2の冪でない ({})", v.qsize));
        }
        Ok(v)
    }
}

// --- ゲストRAMの読み書き (境界を守る小さな手) ---
//
// 表がRAMの外を指していたらNone。**パニックにしない** — 表を書くのはゲストで、
// ゲストのバグでホストが死ぬのは筋が通らない

fn r16(ram: &[u8], at: u64) -> Option<u16> {
    let at = usize::try_from(at).ok()?;
    Some(u16::from_le_bytes([*ram.get(at)?, *ram.get(at + 1)?]))
}

fn r32(ram: &[u8], at: u64) -> Option<u32> {
    let at = usize::try_from(at).ok()?;
    Some(u32::from_le_bytes([
        *ram.get(at)?,
        *ram.get(at + 1)?,
        *ram.get(at + 2)?,
        *ram.get(at + 3)?,
    ]))
}

fn w16(ram: &mut [u8], at: u64, v: u16) {
    if let Ok(at) = usize::try_from(at) {
        if let Some(dst) = ram.get_mut(at..at + 2) {
            dst.copy_from_slice(&v.to_le_bytes());
        }
    }
}

fn w32(ram: &mut [u8], at: u64, v: u32) {
    if let Ok(at) = usize::try_from(at) {
        if let Some(dst) = ram.get_mut(at..at + 4) {
            dst.copy_from_slice(&v.to_le_bytes());
        }
    }
}
