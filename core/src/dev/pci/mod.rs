//! PCI — **装置を数える仕組み**。
//!
//! ISAには装置を列挙する方法が無かった。「COM1は0x3F8」「NE2000は0x300」と
//! 全メーカーが定数で示し合わせ、ぶつかったらジャンパで逃げる世界である
//! ([mem::bus](../../mem/bus.rs) のデコーダが `match` で足りるのはそのため)。
//!
//! PCIが持ち込んだのは**設定空間 (configuration space)** である。バス上の
//! どの位置に何が挿さっているかをソフトから読み出せるようになり、番地は
//! firmwareかOSが後から割り当てるものになった。`lspci` が見ているのはここで、
//! 現代PCの「挿せば見える」はこの1枚の表から始まっている。
//!
//! ## 機構#1 (Configuration Mechanism #1)
//!
//! 設定空間はメモリでもI/Oでもない**第3の空間**で、2つのポートから覗く:
//!
//! ```text
//!   0xCF8 (32bit書込)  アドレス  bit31=有効 | bus[23:16] | dev[15:11] | fun[10:8] | reg[7:2]
//!   0xCFC (32bit読書)  データ    上で指した4バイト
//! ```
//!
//! 下位2bitが常に0なのは、**設定空間が32bit語の並び**として定義されているため。
//! バイト単位で触りたいときは 0xCFC からのずれで指す (0xCFD なら reg+1)。
//!
//! ## BARの大きさの測り方
//!
//! 装置が欲しい番地の幅は、レジスタに「全部1」を書いてから読み返すと分かる。
//! 装置は**自分が使わない下位ビットだけを残して0を返す**ので、読めた値を
//! 反転して+1すれば大きさになる。番地を割り当てる側 (firmware / OS) は
//! この手順で必要な幅を知ってから、空いている場所を配る。
//!
//! 設定空間は「読めるが書けない」欄が多い。ベンダIDや装置IDを書き換えられたら
//! 装置の身元が変わってしまうので、**書き込みを受け付ける欄を明示的に選ぶ**のが
//! 実装の肝である (ここを素通しにすると、OSのBAR測定が嘘の値を読む)。

/// 設定空間のオフセット (よく使うものだけ)
pub mod reg {
    pub const VENDOR_ID: usize = 0x00;
    pub const DEVICE_ID: usize = 0x02;
    pub const COMMAND: usize = 0x04;
    pub const STATUS: usize = 0x06;
    pub const REVISION: usize = 0x08;
    pub const CLASS_CODE: usize = 0x09; // 3バイト (prog-if, subclass, class)
    pub const HEADER_TYPE: usize = 0x0E;
    pub const BAR0: usize = 0x10;
    pub const SUBSYS_VENDOR: usize = 0x2C;
    pub const SUBSYS_ID: usize = 0x2E;
    pub const INTERRUPT_LINE: usize = 0x3C;
    pub const INTERRUPT_PIN: usize = 0x3D;
}

/// COMMANDレジスタのビット (OSが装置の口を開けるときに立てる)
pub mod command {
    /// I/O空間への応答を許す
    pub const IO_SPACE: u16 = 1 << 0;
    /// メモリ空間への応答を許す
    pub const MEMORY_SPACE: u16 = 1 << 1;
    /// バスマスタ (装置側から転送を始めてよい)
    pub const BUS_MASTER: u16 = 1 << 2;
}

/// BARの種類と大きさ。**0は「そのBARは無い」**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Bar {
    /// 幅 (バイト)。2のべき乗。0なら未実装
    pub size: u32,
    /// I/O空間のBARか (falseならメモリ空間)
    pub io: bool,
}

/// バスに挿さっている1つの機能 (function)。
///
/// 設定空間256バイトを丸ごと持つ。**読みは素通し、書きは選んだ欄だけ**という
/// 作りにしてあるのは、OSがBARの幅を測るときに「書いた値がそのまま読める」と
/// 幅を測れなくなるからである
#[derive(Debug, Clone)]
pub struct PciFunction {
    cfg: [u8; 256],
    bars: [Bar; 6],
}

impl PciFunction {
    /// 身元だけを決めて作る。BARは [`with_bar`](Self::with_bar) で足す
    pub fn new(vendor: u16, device: u16, class: u8, subclass: u8, prog_if: u8) -> Self {
        let mut f = Self {
            cfg: [0; 256],
            bars: [Bar::default(); 6],
        };
        f.cfg[reg::VENDOR_ID..reg::VENDOR_ID + 2].copy_from_slice(&vendor.to_le_bytes());
        f.cfg[reg::DEVICE_ID..reg::DEVICE_ID + 2].copy_from_slice(&device.to_le_bytes());
        f.cfg[reg::CLASS_CODE] = prog_if;
        f.cfg[reg::CLASS_CODE + 1] = subclass;
        f.cfg[reg::CLASS_CODE + 2] = class;
        f
    }

    /// BARを1つ足す。`base` は firmware が割り当てた番地 (実機の起動時と同じで、
    /// **OSが来る前に誰かが配ってある**のが普通の姿)
    pub fn with_bar(mut self, index: usize, bar: Bar, base: u32) -> Self {
        self.bars[index] = bar;
        let raw = base | if bar.io { 1 } else { 0 };
        let at = reg::BAR0 + index * 4;
        self.cfg[at..at + 4].copy_from_slice(&raw.to_le_bytes());
        self
    }

    /// 割り込み線 (IRQ番号) を書いておく。**firmwareが配るもの**で、
    /// OSはここを読んでハンドラを繋ぐ
    pub fn with_irq(mut self, line: u8, pin: u8) -> Self {
        self.cfg[reg::INTERRUPT_LINE] = line;
        self.cfg[reg::INTERRUPT_PIN] = pin;
        self
    }

    /// サブシステムID。**装置の型番を細かく見るドライバがある** (RTL8029もそう)
    pub fn with_subsystem(mut self, vendor: u16, id: u16) -> Self {
        self.cfg[reg::SUBSYS_VENDOR..reg::SUBSYS_VENDOR + 2].copy_from_slice(&vendor.to_le_bytes());
        self.cfg[reg::SUBSYS_ID..reg::SUBSYS_ID + 2].copy_from_slice(&id.to_le_bytes());
        self
    }

    /// COMMANDレジスタの今の値
    pub fn command(&self) -> u16 {
        u16::from_le_bytes([self.cfg[reg::COMMAND], self.cfg[reg::COMMAND + 1]])
    }

    /// このBARが今指している番地 (下位の種別ビットを落としたもの)
    pub fn bar_base(&self, index: usize) -> u32 {
        let at = reg::BAR0 + index * 4;
        let raw = u32::from_le_bytes([
            self.cfg[at],
            self.cfg[at + 1],
            self.cfg[at + 2],
            self.cfg[at + 3],
        ]);
        if self.bars[index].io {
            raw & !0x3
        } else {
            raw & !0xF
        }
    }

    /// I/OのBARがこのポートを覆っているか。覆っていればBARの先頭からのずれを返す。
    /// **COMMANDのI/O許可が下りていなければ、装置は名乗り出ない** (実機と同じ)
    pub fn io_hit(&self, port: u16) -> Option<u16> {
        if self.command() & command::IO_SPACE == 0 {
            return None;
        }
        for i in 0..6 {
            let bar = self.bars[i];
            if bar.size == 0 || !bar.io {
                continue;
            }
            let base = self.bar_base(i) as u16;
            if port >= base && (port as u32) < base as u32 + bar.size as u32 {
                return Some(port - base);
            }
        }
        None
    }

    fn read_u32(&self, reg: usize) -> u32 {
        let at = reg & 0xFC;
        u32::from_le_bytes([
            self.cfg[at],
            self.cfg[at + 1],
            self.cfg[at + 2],
            self.cfg[at + 3],
        ])
    }

    /// 設定空間への書き込み。**受け付ける欄を明示的に選ぶ**
    fn write_u32(&mut self, reg: usize, val: u32) {
        let at = reg & 0xFC;
        let put = |cfg: &mut [u8; 256], v: u32| cfg[at..at + 4].copy_from_slice(&v.to_le_bytes());
        match at {
            // COMMAND (下位16bit) だけ書ける。STATUS (上位) は装置が立てる側
            reg::COMMAND => {
                let keep = self.read_u32(at) & 0xFFFF_0000;
                put(&mut self.cfg, keep | (val & 0xFFFF));
            }
            // BAR: **幅の測定に答える**。使わない下位ビットは常に0を返す
            0x10..=0x27 => {
                let i = (at - reg::BAR0) / 4;
                let bar = self.bars[i];
                if bar.size == 0 {
                    return; // 無いBARは書いても0のまま (OSはこれで不在を知る)
                }
                let mask = !(bar.size - 1);
                let kind = if bar.io { 1 } else { 0 };
                put(&mut self.cfg, (val & mask) | kind);
            }
            // 割り込み線はfirmwareが配り、OSが書き換えることもある
            0x3C => {
                let keep = self.read_u32(at) & 0xFFFF_0000;
                put(&mut self.cfg, keep | (val & 0xFFFF));
            }
            // 身元 (ベンダ/装置/クラス) とその他は読み取り専用。黙って捨てる
            _ => {}
        }
    }
}

/// ホストブリッジ — 設定空間の窓口 (0xCF8 / 0xCFC)。
///
/// 実機ではCPUバスとPCIバスの間に立つチップだが、エミュレータでは
/// **「設定空間を配る係」**だけが仕事になる
#[derive(Debug, Clone, Default)]
pub struct PciHost {
    /// 0xCF8 に書かれた値 (次にどこを覗くか)
    addr: u32,
    /// バス0のスロット。**32個は規格の上限**で、それ以上はブリッジの向こう側になる
    slots: [Option<PciFunction>; 32],
}

/// 設定アドレスポート (32bit)
pub const CONFIG_ADDRESS: u16 = 0xCF8;
/// 設定データポート (32bit。バイトで触るときは +1..+3 のずれで指す)
pub const CONFIG_DATA: u16 = 0xCFC;

impl PciHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// スロットに挿す
    pub fn plug(&mut self, slot: usize, f: PciFunction) {
        self.slots[slot] = Some(f);
    }

    pub fn slot(&self, slot: usize) -> Option<&PciFunction> {
        self.slots.get(slot).and_then(|s| s.as_ref())
    }

    pub fn slot_mut(&mut self, slot: usize) -> Option<&mut PciFunction> {
        self.slots.get_mut(slot).and_then(|s| s.as_mut())
    }

    /// 0xCF8 が今指している (bus, device, function, reg)。有効ビットが
    /// 立っていなければ None — **無効なアドレスへのデータ読みは 0xFFFF_FFFF**
    fn target(&self) -> Option<(u8, usize, u8, usize)> {
        if self.addr & 0x8000_0000 == 0 {
            return None;
        }
        let bus = ((self.addr >> 16) & 0xFF) as u8;
        let dev = ((self.addr >> 11) & 0x1F) as usize;
        let fun = ((self.addr >> 8) & 0x07) as u8;
        let reg = (self.addr & 0xFC) as usize;
        Some((bus, dev, fun, reg))
    }

    /// 指しているスロットの設定空間 (4バイト)。
    /// **居ないところは 0xFFFF_FFFF** — これがPCIの「不在」の返事で、
    /// OSはベンダIDが全部1なら空きスロットと判断する
    fn config_read(&self) -> u32 {
        let Some((bus, dev, fun, reg)) = self.target() else {
            return 0xFFFF_FFFF;
        };
        // バス0だけを実装する (ブリッジの向こうはまだ無い)。多機能装置も未対応
        if bus != 0 || fun != 0 {
            return 0xFFFF_FFFF;
        }
        match self.slots[dev].as_ref() {
            Some(f) => f.read_u32(reg),
            None => 0xFFFF_FFFF,
        }
    }

    fn config_write(&mut self, val: u32) {
        let Some((bus, dev, fun, reg)) = self.target() else {
            return;
        };
        if bus != 0 || fun != 0 {
            return;
        }
        if let Some(f) = self.slots[dev].as_mut() {
            f.write_u32(reg, val);
        }
    }

    /// 0xCF8-0xCFF の読み。`size` は1/2/4バイト
    pub fn io_read(&self, port: u16, size: u8) -> u32 {
        if (CONFIG_ADDRESS..CONFIG_ADDRESS + 4).contains(&port) {
            let shift = (port - CONFIG_ADDRESS) * 8;
            return (self.addr >> shift) & mask_of(size);
        }
        // データ側は**ポートのずれがそのままレジスタ内のずれ**になる。
        // 0xCFD をバイトで読めば reg+1 のバイトが返る (Linuxのpci_conf1がこうする)
        let shift = (port - CONFIG_DATA) * 8;
        (self.config_read() >> shift) & mask_of(size)
    }

    /// 0xCF8-0xCFF の書き
    pub fn io_write(&mut self, port: u16, val: u32, size: u8) {
        if (CONFIG_ADDRESS..CONFIG_ADDRESS + 4).contains(&port) {
            let shift = (port - CONFIG_ADDRESS) * 8;
            let m = mask_of(size) << shift;
            self.addr = (self.addr & !m) | ((val << shift) & m);
            return;
        }
        let shift = (port - CONFIG_DATA) * 8;
        let m = mask_of(size) << shift;
        let cur = self.config_read();
        self.config_write((cur & !m) | ((val << shift) & m));
    }

    /// このポートを名乗り出るスロットを探す。**ここがPCIの動的な振り分け**で、
    /// ISAの定数`match`と違い、番地はBARで動く (ADR-0017 決定3)
    pub fn io_hit(&self, port: u16) -> Option<(usize, u16)> {
        for (i, s) in self.slots.iter().enumerate() {
            if let Some(f) = s {
                if let Some(off) = f.io_hit(port) {
                    return Some((i, off));
                }
            }
        }
        None
    }
}

fn mask_of(size: u8) -> u32 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => 0xFFFF_FFFF,
    }
}

/// 440FX相当のホストブリッジ。**スロット0に居るのが慣例**で、
/// OSはここを見てPCIバスの存在を確かめる
pub fn host_bridge() -> PciFunction {
    // 8086:1237 = Intel 440FX。QEMUの既定でもあり、Linuxが素直に認識する
    PciFunction::new(0x8086, 0x1237, 0x06, 0x00, 0x00)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> PciHost {
        let mut h = PciHost::new();
        h.plug(0, host_bridge());
        h
    }

    /// 0xCF8 で指して 0xCFC で読む、という機構#1の一往復
    #[test]
    fn config_mechanism_one_roundtrip() {
        let h = host();
        let mut h = h;
        h.io_write(CONFIG_ADDRESS, 0x8000_0000, 4); // bus0 dev0 fun0 reg0
        assert_eq!(h.io_read(CONFIG_DATA, 4), 0x1237_8086, "ベンダIDと装置ID");
        // バイトで覗いても同じ場所が見える (ポートのずれ = レジスタ内のずれ)
        assert_eq!(h.io_read(CONFIG_DATA, 1), 0x86);
        assert_eq!(h.io_read(CONFIG_DATA + 1, 1), 0x80);
        assert_eq!(h.io_read(CONFIG_DATA + 2, 2), 0x1237);
    }

    /// **空きスロットは全部1を返す。** OSはこれで不在を知る
    #[test]
    fn empty_slot_reads_all_ones() {
        let mut h = host();
        h.io_write(CONFIG_ADDRESS, 0x8000_0000 | (3 << 11), 4);
        assert_eq!(h.io_read(CONFIG_DATA, 4), 0xFFFF_FFFF);
        // 有効ビットが立っていないアドレスも不在の顔をする
        h.io_write(CONFIG_ADDRESS, 0, 4);
        assert_eq!(h.io_read(CONFIG_DATA, 4), 0xFFFF_FFFF);
    }

    /// **身元は書き換えられない。** ここが素通しだとOSが別の装置だと思い込む
    #[test]
    fn identity_is_read_only() {
        let mut h = host();
        h.io_write(CONFIG_ADDRESS, 0x8000_0000, 4);
        h.io_write(CONFIG_DATA, 0xDEAD_BEEF, 4);
        assert_eq!(h.io_read(CONFIG_DATA, 4), 0x1237_8086);
    }

    /// BARの幅の測定 — 全部1を書いて読み返すと、使わない下位だけが残る
    #[test]
    fn bar_sizing_reports_width() {
        let mut h = PciHost::new();
        h.plug(
            2,
            PciFunction::new(0x10EC, 0x8029, 0x02, 0x00, 0x00).with_bar(
                0,
                Bar { size: 32, io: true },
                0xC000,
            ),
        );
        let addr = 0x8000_0000 | (2 << 11) | reg::BAR0 as u32;
        h.io_write(CONFIG_ADDRESS, addr, 4);
        assert_eq!(h.io_read(CONFIG_DATA, 4), 0xC001, "I/O BARは下位bitが1");

        h.io_write(CONFIG_DATA, 0xFFFF_FFFF, 4);
        let probe = h.io_read(CONFIG_DATA, 4);
        // 幅32バイト → 下位5bitは0。反転して+1すると32が出る
        assert_eq!(probe & !0x3, 0xFFFF_FFE0);
        assert_eq!((!(probe & !0x3)).wrapping_add(1), 32);

        // 測り終えたら番地を書き戻せる
        h.io_write(CONFIG_DATA, 0xD000, 4);
        assert_eq!(h.io_read(CONFIG_DATA, 4), 0xD001);
    }

    /// 無いBARは書いても0のまま (OSはこれで「このBARは無い」と知る)
    #[test]
    fn absent_bar_stays_zero() {
        let mut h = host();
        h.io_write(CONFIG_ADDRESS, 0x8000_0000 | reg::BAR0 as u32, 4);
        h.io_write(CONFIG_DATA, 0xFFFF_FFFF, 4);
        assert_eq!(h.io_read(CONFIG_DATA, 4), 0);
    }

    /// **I/O許可が下りるまで装置は名乗り出ない。** ここを見落とすと、
    /// OSが番地を配り替えている途中の一瞬に、古い番地で応答してしまう
    #[test]
    fn io_hit_needs_command_bit() {
        let mut h = PciHost::new();
        h.plug(
            2,
            PciFunction::new(0x10EC, 0x8029, 0x02, 0x00, 0x00).with_bar(
                0,
                Bar { size: 32, io: true },
                0xC000,
            ),
        );
        assert_eq!(h.io_hit(0xC000), None, "許可前は名乗らない");

        let addr = 0x8000_0000 | (2 << 11) | reg::COMMAND as u32;
        h.io_write(CONFIG_ADDRESS, addr, 4);
        h.io_write(CONFIG_DATA, command::IO_SPACE as u32, 4);

        assert_eq!(h.io_hit(0xC000), Some((2, 0)));
        assert_eq!(h.io_hit(0xC01F), Some((2, 0x1F)), "窓の端まで届く");
        assert_eq!(h.io_hit(0xC020), None, "窓の外は名乗らない");
        assert_eq!(h.io_hit(0xBFFF), None);
    }

    /// 番地を配り替えたら、名乗り出る場所も動く
    #[test]
    fn io_window_follows_the_bar() {
        let mut h = PciHost::new();
        h.plug(
            2,
            PciFunction::new(0x10EC, 0x8029, 0x02, 0x00, 0x00)
                .with_bar(0, Bar { size: 32, io: true }, 0xC000)
                .with_irq(11, 1),
        );
        let cmd = 0x8000_0000 | (2 << 11) | reg::COMMAND as u32;
        h.io_write(CONFIG_ADDRESS, cmd, 4);
        h.io_write(CONFIG_DATA, command::IO_SPACE as u32, 4);
        assert_eq!(h.io_hit(0xC000), Some((2, 0)));

        let bar = 0x8000_0000 | (2 << 11) | reg::BAR0 as u32;
        h.io_write(CONFIG_ADDRESS, bar, 4);
        h.io_write(CONFIG_DATA, 0xE000, 4);
        assert_eq!(h.io_hit(0xC000), None, "古い番地では応答しない");
        assert_eq!(h.io_hit(0xE000), Some((2, 0)));

        assert_eq!(h.slot(2).unwrap().cfg[reg::INTERRUPT_LINE], 11);
    }
}
