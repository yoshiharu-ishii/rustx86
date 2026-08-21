//! メモリ — 線形→物理の変換 (ページング・TLB) と、読み書きの経路。
//!
//! 読み出しは最も回数の多い経路なので分岐を足さない、書き込み側に
//! 仕掛けを寄せる (VRAM検出・自己書き換え検出)、という非対称が設計の芯。
//!
//! 棚割り (全部 `impl Machine` なので、呼び出し側にファイル分けは見えない):
//! - [`paging`] — 線形→物理の変換・TLB・#PFの記録
//! - [`rw`] — CPUが触る正規の読み書き (read8/16/32・write8/16/32) とテキストVRAM
//! - [`fast`] — translate-first の速い道 (平坦セグメント・ページ内だけ Some)
//! - [`jit`] — 生成コードから呼ばれる記録しないアクセス (脱出モデル)
//! - [`io`] — I/Oポート空間の振り分け (ISA定数match + PCI BAR探索)
//! - ここ (mod.rs) — 物理アドレス直接アクセスと起動用の小物

mod fast;
mod io;
mod jit;
mod paging;
mod rw;

use crate::{cpu, Machine};

impl Machine {
    /// RAMのバイト数 (= 実際の確保量)
    pub fn ram_bytes(&self) -> usize {
        self.mem.len()
    }

    /// Linuxへ申告するリニアフレームバッファを挿す (起動前に呼ぶ)。
    /// RAMの末尾1MBを切り出し、e820で予約して efifb に掴ませる
    pub fn lfb_enable(&mut self) {
        self.lfb = Some(crate::boot::bzimage::Lfb::at_top_of(self.mem.len() as u64));
    }

    /// 解像度を指定して申告する (X機の 1024×768 など)
    pub fn lfb_enable_sized(&mut self, width: u16, height: u16) {
        self.lfb = Some(crate::boot::bzimage::Lfb::sized_at_top_of(
            self.mem.len() as u64,
            width,
            height,
        ));
    }

    /// LFBの中身 (申告していなければ空)。FB同様**ただのRAMの窓**で、
    /// 表示側が毎フレーム読む
    pub fn lfb_frame(&self) -> &[u8] {
        match self.lfb {
            Some(l) => {
                let b = l.base as usize;
                &self.mem[b..b + l.frame_bytes() as usize]
            }
            None => &[],
        }
    }

    /// 物理アドレスへ書く (変換しない)。テストや装置初期化用
    pub fn write_phys8(&mut self, pa: u32, val: u8) {
        if let Some(b) = self.mem.get_mut(pa as usize) {
            *b = val;
        }
        // 超えたら捨てる (未マップへの書き込みは実機でも消える)
        // コードを控えたページへの書き込みは写しを無効化 (自己書き換え対策)
        self.dcache.note_write(pa);
    }

    pub fn write_phys32(&mut self, pa: u32, val: u32) {
        for (i, b) in val.to_le_bytes().iter().enumerate() {
            self.write_phys8(pa.wrapping_add(i as u32), *b);
        }
    }

    /// 物理アドレスから読む (変換しない)。ページテーブルの歩きと、
    /// 物理番地で語る装置・テストが使う
    pub fn read_phys8(&self, pa: u32) -> u8 {
        // RAMを超えた番地は未マップ。実機のバスと同じく 0xFF を返す (折り返さない)。
        // リアルモードのアドレスは cpu::lin が 1MB に丸めてから来るので、
        // 16bit機 (1MB) でここが 0xFF を返すことはない
        *self.mem.get(pa as usize).unwrap_or(&0xFF)
    }

    pub fn read_phys32(&self, pa: u32) -> u32 {
        // RAMに収まるなら4バイトを一気に読む (ページウォークの熱い経路)
        let a = pa as usize;
        if a + 4 <= self.mem.len() {
            u32::from_le_bytes([
                self.mem[a],
                self.mem[a + 1],
                self.mem[a + 2],
                self.mem[a + 3],
            ])
        } else {
            u32::from_le_bytes([
                self.read_phys8(pa),
                self.read_phys8(pa.wrapping_add(1)),
                self.read_phys8(pa.wrapping_add(2)),
                self.read_phys8(pa.wrapping_add(3)),
            ])
        }
    }

    /// 生のメモリスライスへの参照 (REP一括処理の宛先)。
    /// VRAMやデバッガの都合は呼び出し側が事前に外す
    pub(crate) fn mem_slice_mut(&mut self) -> &mut [u8] {
        &mut self.mem
    }

    /// ブートセクタ (512バイト) を0x7C00に配置し、CS:IP=0000:7C00から実行開始
    pub fn load_boot_sector(&mut self, sector: &[u8]) -> Result<(), String> {
        if sector.len() != 512 {
            return Err(format!(
                "boot sector must be 512 bytes, got {}",
                sector.len()
            ));
        }
        if sector[510] != 0x55 || sector[511] != 0xAA {
            return Err("missing boot signature 0x55AA".into());
        }
        self.power_on_self_test();
        self.mem[0x7C00..0x7E00].copy_from_slice(sector);
        self.cpu.set_cs_ip(0x0000, 0x7C00);
        self.cpu.regs[cpu::DX] = 0x0080; // DL = ブートドライブ番号
        Ok(())
    }

    /// ハードウェア割り込みベクタを直接立てる (PICを介さない経路。テスト用)
    pub fn raise_irq(&mut self, vector: u8) {
        self.pending_irq = Some(vector);
    }
}
