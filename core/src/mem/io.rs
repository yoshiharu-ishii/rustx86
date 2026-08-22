//! I/Oポート空間の振り分け — `in`/`out` が触るもう1つのアドレス空間。
//!
//! 番地が定数のISA装置は [`bus::decode_io`] の`match`で即決し、
//! 番地がBARで動くPCI装置だけ実行時に探す (`pci_io_read`/`pci_slot_*`)。

use crate::{bus, debug, IoTarget, Machine};

impl Machine {
    /// ポートから読む。
    ///
    /// 未接続のポートは **0xFF** を返す。実機のISAバスは誰もドライブしないと
    /// プルアップで全ビットが立つためで、OSはこの値を見て「装置が居ない」と
    /// 判断する。ここで panic すると装置探索の段階で止まってしまう
    pub fn io_read8(&mut self, port: u16) -> u8 {
        let val = self.io_read8_inner(port);
        // **読んだ値まで残す。** 「装置が何を答えたか」が分からないと、
        // OSがなぜその判断をしたのかを追えない
        if self.dbg.on && self.dbg.io_read.contains(&port) {
            self.dbg.stop = Some(debug::Stop::ReadIo {
                port,
                val,
                at: self.dbg.at,
            });
        }
        val
    }

    fn io_read8_inner(&mut self, port: u16) -> u8 {
        match bus::decode_io(port) {
            IoTarget::Pic { slave } => {
                let p = &self.devices.pic[slave as usize];
                if port & 1 == 0 {
                    p.read_command()
                } else {
                    p.read_data()
                }
            }
            IoTarget::Pit => {
                let idx = (port & 3) as usize;
                if idx == 3 {
                    0xFF
                } else {
                    self.devices.pit.read_counter(idx)
                }
            }
            IoTarget::Keyboard => {
                if port == 0x64 {
                    self.devices.keyboard.read_status()
                } else {
                    self.devices.keyboard.read_data()
                }
            }
            IoTarget::Uart => self.devices.uart.read(port & 7),
            IoTarget::Cmos => {
                if port == 0x71 {
                    self.devices.cmos.read_data()
                } else {
                    0xFF
                }
            }
            IoTarget::Crtc => {
                if port == 0x3D5 {
                    self.devices.crtc.read_data()
                } else {
                    0xFF
                }
            }
            IoTarget::VgaSeq => {
                if port == 0x3C5 {
                    self.devices.vga.seq_read_data()
                } else {
                    0xFF
                }
            }
            IoTarget::VgaGc => {
                if port == 0x3CF {
                    self.devices.vga.gc_read_data()
                } else {
                    0xFF
                }
            }
            IoTarget::Dac => match port {
                0x3C6 => self.devices.dac.read_pel_mask(),
                0x3C8 => self.devices.dac.read_write_index(),
                0x3C9 => self.devices.dac.read_data(),
                _ => 0xFF, // 0x3C7の読みは状態レジスタ (未実装。書き専用として扱う)
            },
            IoTarget::VideoStatus => self.video_status(),
            IoTarget::SystemControl => {
                // bit4 をトグルし続ける。OSがリフレッシュ矩形波を数えて
                // 時間を測る古い手口に付き合うため
                self.devices.sysctl ^= 0x10;
                self.devices.sysctl
            }
            IoTarget::Net => match &mut self.devices.net {
                // **PCI機ではISAの0x300窓は開かない。** カードはPCIスロット側に
                // 居て、番地はBARが決める — 同じ実体が両方の窓で応えると、
                // OSが2枚あると数えてしまう
                Some(net) if !self.profile.has_pci => net.read(port - bus::isa::NET_BASE),
                // カードが挿さっていなければ、ただの空きスロットである
                _ => {
                    self.unhandled_io.insert(port);
                    0xFF
                }
            },
            IoTarget::PciConfig => match &self.devices.pci {
                Some(pci) => pci.io_read(port, 1) as u8,
                None => {
                    self.unhandled_io.insert(port);
                    0xFF
                }
            },
            IoTarget::Unmapped => self.pci_io_read(port),
        }
    }

    /// 0x3DA 入力状態レジスタ1 — 垂直帰線 (bit3) と表示ブランク (bit0)。
    ///
    /// レジスタの実体は無く、**機械の時計 (tsc) から合成する**。ゲームは
    /// 「帰線を待ってから描き換える」ループでテンポを取るので、ここが常に
    /// 同じ値だと永久に待ち続ける。tscだけの関数なので命令数決定性は無傷。
    ///
    /// 校正の原点は PIT_CLOCKS_PER_TICK と同じ「64命令 ≒ 1 PITクロック」。
    /// mode 13h の垂直同期は70Hz = 1193182/70 ≒ 17045 PITクロック/フレーム
    fn video_status(&mut self) -> u8 {
        const FRAME: u64 = 17045 * 64; // ≒ 1/70秒ぶんの命令数
        const VRETRACE: u64 = FRAME * 4 / 100; // 帰線はフレーム末尾の約4%
        const LINE: u64 = FRAME / 449; // 400走査線 + 帰線期間 = 449本
        const HBLANK: u64 = LINE / 5;
        let t = self.cpu.tsc % FRAME;
        let mut st = 0u8;
        // bit0 は「表示していない」— 垂直・水平どちらのブランクでも立つ
        if t >= FRAME - VRETRACE {
            st |= 0x08 | 0x01;
        }
        if (t % LINE) >= LINE - HBLANK {
            st |= 0x01;
        }
        st
    }

    /// PCIの窓に落ちるか。**ISAの定数`match`で名乗り手が居なかったときだけ**
    /// ここへ来る — 番地がBARで動く装置は、実行時に探すしかない
    fn pci_io_read(&mut self, port: u16) -> u8 {
        if let Some(pci) = &self.devices.pci {
            if let Some((slot, off)) = pci.io_hit(port) {
                return self.pci_slot_read(slot, off);
            }
        }
        self.unhandled_io.insert(port);
        0xFF
    }

    /// PCIの装置への読み。**挿さっている装置ごとの分岐はここ1箇所**
    fn pci_slot_read(&mut self, slot: usize, off: u16) -> u8 {
        match slot {
            // RTL8029: 皮はPCIでも中身はISA版と同じDP8390
            crate::dev::card::rtl8029::NET_SLOT => match &mut self.devices.net {
                Some(net) => net.read(off),
                None => 0xFF,
            },
            crate::dev::card::virtio_blk::BLK_SLOT => match &mut self.devices.blk {
                Some(blk) => blk.vio.read(off),
                None => 0xFF,
            },
            _ => 0xFF,
        }
    }

    /// PCIの装置への書き
    fn pci_slot_write(&mut self, slot: usize, off: u16, val: u8) {
        match slot {
            crate::dev::card::rtl8029::NET_SLOT => {
                if let Some(net) = &mut self.devices.net {
                    net.write(off, val);
                }
            }
            crate::dev::card::virtio_blk::BLK_SLOT => {
                if let Some(blk) = &mut self.devices.blk {
                    blk.vio.write(off, val);
                }
            }
            _ => {}
        }
    }

    pub fn io_write8(&mut self, port: u16, val: u8) {
        // POST診断ポート。テストROM (test386) が進行番号を書く — 足跡として残す
        if port == 0x190 {
            self.post_trail.push(val);
        }
        if self.dbg.on && self.dbg.io_write.contains(&port) {
            self.dbg.stop = Some(debug::Stop::WriteIo {
                port,
                val,
                at: self.dbg.at,
            });
        }
        match bus::decode_io(port) {
            IoTarget::Pic { slave } => {
                let p = &mut self.devices.pic[slave as usize];
                if port & 1 == 0 {
                    p.write_command(val)
                } else {
                    p.write_data(val)
                }
            }
            IoTarget::Pit => {
                let idx = (port & 3) as usize;
                if idx == 3 {
                    self.devices.pit.write_control(val)
                } else {
                    self.devices.pit.write_counter(idx, val)
                }
            }
            IoTarget::Keyboard => {
                if port == 0x64 {
                    self.devices.keyboard.write_command(val)
                } else {
                    self.devices.keyboard.write_data(val)
                }
            }
            IoTarget::Uart => self.devices.uart.write(port & 7, val),
            IoTarget::Cmos => {
                if port == 0x70 {
                    self.devices.cmos.write_index(val)
                } else {
                    self.devices.cmos.write_data(val)
                }
            }
            IoTarget::VgaSeq => {
                if port == 0x3C4 {
                    self.devices.vga.seq_write_index(val);
                } else {
                    let ev = self.devices.vga.seq_write_data(val);
                    self.vga_event(ev);
                }
            }
            IoTarget::VgaGc => {
                if port == 0x3CE {
                    self.devices.vga.gc_write_index(val);
                } else {
                    let ev = self.devices.vga.gc_write_data(val);
                    self.vga_event(ev);
                }
            }
            IoTarget::Crtc => {
                if port == 0x3D4 {
                    self.devices.crtc.write_index(val)
                } else {
                    // 表示開始位置が動いたら、メモリは変わらなくても**画面は変わる**
                    if matches!(self.devices.crtc.index(), 0x0C | 0x0D) {
                        self.vram_dirty = true;
                    }
                    self.devices.crtc.write_data(val)
                }
            }
            IoTarget::Dac => match port {
                0x3C6 => self.devices.dac.write_pel_mask(val),
                0x3C7 => self.devices.dac.write_read_index(val),
                0x3C8 => self.devices.dac.write_write_index(val),
                _ => self.devices.dac.write_data(val),
            },
            // 0x3DA への書きはVGAの機能制御レジスタ。読む者が居ないので受けて捨てる
            // (書いた事実は unhandled_io に残し、使うソフトが現れたら台帳から取り出す)
            IoTarget::VideoStatus => {
                self.unhandled_io.insert(port);
            }
            IoTarget::SystemControl => self.devices.sysctl = val,
            IoTarget::Net => match &mut self.devices.net {
                Some(net) if !self.profile.has_pci => net.write(port - bus::isa::NET_BASE, val),
                _ => {
                    self.unhandled_io.insert(port);
                }
            },
            IoTarget::PciConfig => match &mut self.devices.pci {
                Some(pci) => pci.io_write(port, u32::from(val), 1),
                None => {
                    self.unhandled_io.insert(port);
                }
            },
            IoTarget::Unmapped => {
                if let Some(pci) = &self.devices.pci {
                    if let Some((slot, off)) = pci.io_hit(port) {
                        self.pci_slot_write(slot, off, val);
                        return;
                    }
                }
                self.unhandled_io.insert(port);
            }
        }
    }
}
