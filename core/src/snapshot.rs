//! 機械の状態をまるごと保存し、あとから戻す。
//!
//! 「割り込みが入る直前で止めて保存し、あとで何度でもそこから始める」ができる。
//! 実機では不可能な、エミュレータならではの道具である。
//!
//! ## 形式
//!
//! 素朴なバイナリ列で書く。JSONにしないのは**メモリが大きいから**で、
//! 1MBのバイト配列をJSONの数値配列にすると数MBに膨れる。
//! JSONで束ねるのは呼び出し側 (ブラウザ) の仕事にして、ここは中身だけを返す。
//!
//! メモリとディスクは**連長圧縮 (RLE)** をかける。エミュレートしている
//! メモリはほとんどがゼロなので、1MBが数KBまで縮む。
//!
//! ## 何を保存するか
//!
//! CPUだけでは足りない。**装置の状態も一緒でなければ再開できない**。
//! PICのマスクが失われれば以後の割り込みが来なくなり、PITのカウンタが
//! 戻れば時計が飛ぶ。「状態」とはCPUと装置とメモリの全部である。

const MAGIC: &[u8; 8] = b"RX86SNAP";
/// 形式の版。合わないものは読まない (黙って壊れた状態で動き出さないため)。
/// v2: プロテクトモードの状態 (CR0・GDTR・セグメントの隠しレジスタ) を追加
/// v3: IDTR を追加
/// v4: IP を32bit (EIP) にした
/// v5: TR (タスクレジスタ) を追加
/// v6: CR2/CR3 (ページング) を追加
/// v7: x87の制御語 (fpu_cw) を追加
/// v8: LDTRの隠しレジスタ (base/limit) を追加
/// v9: NE2000 (挿さっていれば) を追加
// v10: NE2000に受信機のラッチ (running) が加わった — STA/STPはコマンドで、
//      crの生値からは走行状態を再現できない
/// v11: x87に80bit原本サイドバンド (raw) が加わった — MMXがここに住む
pub const VERSION: u16 = 12;

/// 順番に書いていくだけの器
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// 長さを付けて生バイト列を書く
    pub fn bytes(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.buf.extend_from_slice(v);
    }
    /// 長さを付けて連長圧縮して書く
    pub fn rle(&mut self, v: &[u8]) {
        let packed = rle_encode(v);
        self.u32(v.len() as u32);
        self.bytes(&packed);
    }
    pub fn opt_u8(&mut self, v: Option<u8>) {
        match v {
            Some(x) => {
                self.bool(true);
                self.u8(x);
            }
            None => self.bool(false),
        }
    }
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

/// 順番に読んでいくだけの器。足りなければエラーにする
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or("スナップショットが壊れている")?;
        let s = self
            .data
            .get(self.pos..end)
            .ok_or("スナップショットが途中で終わっている")?;
        self.pos = end;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> Result<bool, String> {
        Ok(self.u8()? != 0)
    }
    pub fn u16(&mut self) -> Result<u16, String> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    pub fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    pub fn rle(&mut self) -> Result<Vec<u8>, String> {
        let len = self.u32()? as usize;
        let packed = self.bytes()?;
        let out = rle_decode(&packed, len)?;
        Ok(out)
    }
    pub fn opt_u8(&mut self) -> Result<Option<u8>, String> {
        Ok(if self.bool()? { Some(self.u8()?) } else { None })
    }
}

/// 連長圧縮。`[値, 回数]` の並びにする。
///
/// 回数は1バイトなので最大255。凝った方式にしないのは、**狙いがゼロの海を
/// 潰すこと**だけだからである。実際1MBのメモリが数KBになる。
fn rle_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let v = data[i];
        let mut n = 1usize;
        while i + n < data.len() && data[i + n] == v && n < 255 {
            n += 1;
        }
        out.push(v);
        out.push(n as u8);
        i += n;
    }
    out
}

fn rle_decode(packed: &[u8], expect: usize) -> Result<Vec<u8>, String> {
    if !packed.len().is_multiple_of(2) {
        return Err("圧縮データの長さが奇数".into());
    }
    let mut out = Vec::with_capacity(expect);
    for pair in packed.chunks_exact(2) {
        out.extend(std::iter::repeat_n(pair[0], pair[1] as usize));
    }
    if out.len() != expect {
        return Err(format!(
            "展開後の大きさが合わない ({} != {expect})",
            out.len()
        ));
    }
    Ok(out)
}

/// 先頭の印と版を書く
pub fn write_header(w: &mut Writer) {
    w.buf.extend_from_slice(MAGIC);
    w.u16(VERSION);
}

/// 先頭の印と版を確かめる
pub fn read_header(r: &mut Reader) -> Result<(), String> {
    let magic = r.take(8)?;
    if magic != MAGIC {
        return Err("スナップショットではない".into());
    }
    let v = r.u16()?;
    if v != VERSION {
        return Err(format!("形式の版が違う (保存 {v} / 対応 {VERSION})"));
    }
    Ok(())
}

// ---------- Machine 全体の書き出しと復元 ----------
//
// Writer/Reader (上) が「バイト列の刻み方」、ここが「何をどの順で刻むか」。

use crate::{cpu, Disk, Machine, MachineProfile, MEM_SIZE};

impl Machine {
    /// 機械の状態をまるごと書き出す。
    ///
    /// **CPUだけでは足りない。** PICのマスクが失われれば以後の割り込みが
    /// 来なくなり、PITのカウンタが戻れば時計が飛ぶ。装置もメモリも
    /// ディスクも含めて初めて「あの瞬間から再開」ができる。
    pub fn save_state(&self) -> Vec<u8> {
        let mut w = Writer::new();
        write_header(&mut w);

        // CPU
        for r in self.cpu.regs {
            w.u32(r);
        }
        for s in self.cpu.sregs {
            w.u16(s);
        }
        w.u32(self.cpu.ip);
        // 遅延フラグは具現化した値で保存する — スナップショット形式は変わらない
        w.u32(self.cpu.eflags());
        // プロテクトモードの状態 (v2)。隠しレジスタを落とすと、復元した瞬間に
        // 全アドレスが嘘になる — セレクタだけでは base を再構成できない
        w.u32(self.cpu.cr0);
        w.u32(self.cpu.gdtr_base);
        w.u16(self.cpu.gdtr_limit);
        w.u32(self.cpu.idtr_base);
        w.u16(self.cpu.idtr_limit);
        w.u16(self.cpu.tr_sel);
        w.u32(self.cpu.tr_base);
        w.u32(self.cpu.tr_limit);
        w.u32(self.cpu.cr2);
        w.u32(self.cpu.cr3);
        w.u32(self.cpu.cr4); // v7
        w.u16(self.cpu.ldtr_sel); // v7
        w.u32(self.cpu.ldtr_base); // v8
        w.u32(self.cpu.ldtr_limit); // v8
        for d in self.cpu.dr {
            w.u32(d);
        }
        w.u16(self.cpu.fpu_cw); // v7
                                // v10: x87のレジスタスタック (f64裏打ち)
        for r in self.cpu.fpu.regs {
            let b = r.to_bits();
            w.u32(b as u32);
            w.u32((b >> 32) as u32);
        }
        w.u8(self.cpu.fpu.empty); // v10
        w.u8(self.cpu.fpu.top); // v10
        w.u16(self.cpu.fpu.cond); // v10
        for r in self.cpu.fpu.raw {
            // v11: 80bit原本。無し=指数0xFFFFの仮数0…は原本としてあり得るので
            // 有無を1バイトで明示する
            match r {
                Some((mant, se)) => {
                    w.u8(1);
                    w.u32(mant as u32);
                    w.u32((mant >> 32) as u32);
                    w.u16(se);
                }
                None => w.u8(0),
            }
        }
        w.u32(self.cpu.mxcsr); // v7
        for x in self.cpu.xmm {
            w.u32(x as u32);
            w.u32((x >> 32) as u32);
            w.u32((x >> 64) as u32);
            w.u32((x >> 96) as u32);
        }
        w.u32(self.cpu.tsc as u32); // v7 (下位のみ。较正はやり直せるので十分)
        w.u32((self.cpu.tsc >> 32) as u32);
        for h in self.cpu.hidden {
            w.u32(h.base);
            w.u32(h.limit);
            w.u8(h.access);
            w.bool(h.big);
        }

        // 機械の進行状態
        w.bool(self.halted);
        w.opt_u8(self.pending_irq);

        // 装置
        for p in &self.devices.pic {
            p.save(&mut w);
        }
        self.devices.pit.save(&mut w);
        self.devices.uart.save(&mut w);
        self.devices.keyboard.save(&mut w);
        self.devices.cmos.save(&mut w);
        self.devices.crtc.save(&mut w);
        match &self.devices.net {
            Some(net) => {
                w.bool(true);
                net.save(&mut w);
            }
            None => w.bool(false),
        }
        match &self.devices.pci {
            Some(pci) => {
                w.bool(true);
                pci.save(&mut w);
            }
            None => w.bool(false),
        }
        match &self.devices.blk {
            Some(blk) => {
                w.bool(true);
                blk.save(&mut w);
            }
            None => w.bool(false),
        }

        // メモリとディスク (ほとんどがゼロなので連長圧縮で潰れる)
        w.rle(&self.mem);
        match &self.disk {
            Some(d) => {
                w.bool(true);
                w.rle(&d.data);
            }
            None => w.bool(false),
        }
        w.buf
    }

    /// 書き出した状態へ戻す。
    ///
    /// 途中で失敗すると**半端に書き換わった機械**が残るので、
    /// まず新しい機械の上に組み立ててから丸ごと差し替える
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        let mut m = Machine::new();
        let mut r = Reader::new(data);
        read_header(&mut r)?;

        for i in 0..8 {
            m.cpu.regs[i] = r.u32()?;
        }
        for i in 0..6 {
            m.cpu.sregs[i] = r.u16()?;
        }
        m.cpu.ip = r.u32()?;
        m.cpu.set_eflags(r.u32()?);
        m.cpu.cr0 = r.u32()?;
        m.cpu.gdtr_base = r.u32()?;
        m.cpu.gdtr_limit = r.u16()?;
        m.cpu.idtr_base = r.u32()?;
        m.cpu.idtr_limit = r.u16()?;
        m.cpu.tr_sel = r.u16()?;
        m.cpu.tr_base = r.u32()?;
        m.cpu.tr_limit = r.u32()?;
        m.cpu.cr2 = r.u32()?;
        m.cpu.cr3 = r.u32()?;
        m.cpu.cr4 = r.u32()?; // v7
        m.cpu.ldtr_sel = r.u16()?; // v7
        m.cpu.ldtr_base = r.u32()?; // v8
        m.cpu.ldtr_limit = r.u32()?; // v8
        for i in 0..8 {
            m.cpu.dr[i] = r.u32()?;
        }
        m.cpu.fpu_cw = r.u16()?; // v7
        for i in 0..8 {
            let lo = r.u32()? as u64;
            let hi = r.u32()? as u64;
            m.cpu.fpu.regs[i] = f64::from_bits(hi << 32 | lo);
        }
        m.cpu.fpu.empty = r.u8()?;
        m.cpu.fpu.top = r.u8()?;
        m.cpu.fpu.cond = r.u16()?;
        for i in 0..8 {
            // v11: 80bit原本
            m.cpu.fpu.raw[i] = if r.u8()? != 0 {
                let mant = r.u32()? as u64 | (r.u32()? as u64) << 32;
                let se = r.u16()?;
                Some((mant, se))
            } else {
                None
            };
        }
        m.cpu.mxcsr = r.u32()?; // v7
        for i in 0..8 {
            let a = r.u32()? as u128;
            let b = r.u32()? as u128;
            let c = r.u32()? as u128;
            let d = r.u32()? as u128;
            m.cpu.xmm[i] = a | b << 32 | c << 64 | d << 96;
        }
        m.cpu.tsc = r.u32()? as u64 | ((r.u32()? as u64) << 32);
        for i in 0..6 {
            m.cpu.hidden[i] = cpu::SegHidden {
                base: r.u32()?,
                limit: r.u32()?,
                access: r.u8()?,
                big: r.bool()?,
            };
        }

        m.halted = r.bool()?;
        m.pending_irq = r.opt_u8()?;
        // pic_service は派生状態なのでPICから作り直す

        for i in 0..2 {
            m.devices.pic[i].load(&mut r)?;
        }
        m.devices.pit.load(&mut r)?;
        m.devices.uart.load(&mut r)?;
        m.devices.keyboard.load(&mut r)?;
        m.devices.cmos.load(&mut r)?;
        m.devices.crtc.load(&mut r)?;
        m.devices.net = if r.bool()? {
            Some(crate::dev::Dp8390::load(&mut r)?)
        } else {
            None
        };
        m.devices.pci = if r.bool()? {
            Some(crate::bus::pci::PciHost::load(&mut r)?)
        } else {
            None
        };
        m.devices.blk = if r.bool()? {
            Some(crate::dev::VirtioBlk::load(&mut r)?)
        } else {
            None
        };

        // メモリのRLEはサイズを暗黙に持つ。復元した長さがそのままRAMサイズ。
        // 物理マスクは mem.len() を見るので、これで大きい機械もそのまま復元される。
        // 別マシンとして復元したことを覗き窓に映すため profile も合わせる
        let mem = r.rle()?;
        if !mem.len().is_power_of_two() {
            return Err(format!("RAMサイズが2の冪でない ({})", mem.len()));
        }
        m.profile = if mem.len() == MEM_SIZE {
            MachineProfile::PC_16BIT
        } else {
            MachineProfile {
                name: "32bit PC",
                ram_bytes: mem.len(),
                has_fpu: true,
                has_cpuid: true,
                has_pci: m.devices.pci.is_some(),
            }
        };
        m.pic_service = m.devices.pic[0].has_pending();
        m.tlb_flush(); // 復元でメモリもcr3も総入れ替え — 古い写しは無効
        m.mem = crate::GuestRam::from_vec(mem);
        #[cfg(all(unix, not(target_arch = "wasm32")))]
        m.fastmem_init(); // RAM実体が替わった — ミラーもfdから作り直す
                          // デコード済み命令の写しも同じ理由で総入れ替え (RAMサイズも変わりうる)
        m.dcache = cpu::dcache::DecodeCache::new(m.mem.len());
        m.disk = if r.bool()? {
            Some(Disk::from_image(r.rle()?)?)
        } else {
            None
        };

        *self = m;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle_round_trips() {
        for case in [
            vec![],
            vec![0u8; 1000],
            vec![1, 1, 2, 3, 3, 3],
            (0..=255u8).collect::<Vec<_>>(),
            vec![7u8; 300], // 255を跨ぐ
        ] {
            let packed = rle_encode(&case);
            assert_eq!(rle_decode(&packed, case.len()).unwrap(), case);
        }
    }

    /// ゼロの海が実際に縮むこと (これが目的)
    #[test]
    fn zeros_compress_hard() {
        let packed = rle_encode(&vec![0u8; 1 << 20]);
        assert!(packed.len() < 10_000, "1MBのゼロが {} バイト", packed.len());
    }

    #[test]
    fn reader_rejects_truncated_data() {
        let mut w = Writer::new();
        write_header(&mut w);
        w.u32(0xDEAD_BEEF);
        let mut r = Reader::new(&w.buf[..w.buf.len() - 2]);
        read_header(&mut r).unwrap();
        assert!(r.u32().is_err(), "途中で切れたデータを受け入れてはいけない");
    }
}
