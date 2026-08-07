pub mod cpu;

pub use cpu::Cpu;

pub const MEM_SIZE: usize = 1 << 20; // リアルモード 1MB

/// BIOS HLE の入口として予約したセグメント。
///
/// 起動時にIVTの全256エントリを `BIOS_SEG:n` で埋める。実行がここへ来たら
/// バイト列を解釈せずホスト側の関数で肩代わりし、`IRET` で戻る。
///
/// この形にしているのは、**OSがIVTを書き換えた瞬間に自然とHLEが外れる**ため。
/// OSが自分のハンドラを登録したベクタはもう `BIOS_SEG` を指していないので、
/// 何の分岐も足さずに乗っ取りが成立する。実機のBIOSとOSの関係そのものである。
pub const BIOS_SEG: u16 = 0xF000;

/// マシン全体。メモリとBIOS HLE (高位エミュレーション) を持つ。
/// 本物のBIOSは実装せず、INT命令をフックして最小限のサービスだけ提供する。
pub struct Machine {
    pub cpu: Cpu,
    pub mem: Vec<u8>,
    /// 保留中のハードウェア割り込みベクタ。IFが立っている命令境界で受け付ける。
    /// Tier 2a で 8259 PIC がここへ挙手する
    pub pending_irq: Option<u8>,
    /// I/Oポート空間 (64K)。x86はメモリとは別のアドレス空間を持ち、
    /// `IN`/`OUT` 命令だけがここに触れる。8080時代からの名残で、
    /// PIC/PIT/UARTといったISA時代の装置は今もこちら側に居る。
    ///
    /// 今はただのバイト配列。Tier 2a でここを装置への振り分けに置き換える。
    pub ports: Vec<u8>,
    /// INT 10h テレタイプ出力の蓄積 (画面代わり)
    pub console: Vec<u8>,
    pub halted: bool,
}

impl Machine {
    pub fn new() -> Self {
        Self {
            cpu: Cpu::new(),
            mem: vec![0; MEM_SIZE],
            pending_irq: None,
            ports: vec![0; 1 << 16],
            console: Vec::new(),
            halted: false,
        }
    }

    /// ブートセクタ (512バイト) を0x7C00に配置し、CS:IP=0000:7C00から実行開始
    pub fn load_boot_sector(&mut self, sector: &[u8]) -> Result<(), String> {
        if sector.len() != 512 {
            return Err(format!("boot sector must be 512 bytes, got {}", sector.len()));
        }
        if sector[510] != 0x55 || sector[511] != 0xAA {
            return Err("missing boot signature 0x55AA".into());
        }
        self.install_bios_vectors();
        self.mem[0x7C00..0x7E00].copy_from_slice(sector);
        self.cpu.set_cs_ip(0x0000, 0x7C00);
        self.cpu.regs[cpu::DX] = 0x0080; // DL = ブートドライブ番号
        Ok(())
    }

    /// IVTの全256エントリを BIOS HLE の入口で埋める。実BIOSが起動時にやることと同じ。
    ///
    /// OSはこの上から自分のハンドラを書き込んで割り込みを乗っ取る。
    /// DOSが「BIOSのINT 13hをフックして自分の処理を挟んでから元へ流す」
    /// というチェーンを作れるのも、ここが単なるメモリだからである。
    fn install_bios_vectors(&mut self) {
        for n in 0..256u32 {
            self.write16(n * 4, n as u16); // オフセット = ベクタ番号
            self.write16(n * 4 + 2, BIOS_SEG);
        }
    }

    /// ハードウェア割り込みを立てる (PICの代役)。IFが立つまで保留される
    pub fn raise_irq(&mut self, vector: u8) {
        self.pending_irq = Some(vector);
    }

    pub fn read8(&self, addr: u32) -> u8 {
        self.mem[(addr as usize) & (MEM_SIZE - 1)]
    }

    pub fn write8(&mut self, addr: u32, val: u8) {
        self.mem[(addr as usize) & (MEM_SIZE - 1)] = val;
    }

    pub fn read16(&self, addr: u32) -> u16 {
        self.read8(addr) as u16 | (self.read8(addr.wrapping_add(1)) as u16) << 8
    }

    pub fn write16(&mut self, addr: u32, val: u16) {
        self.write8(addr, val as u8);
        self.write8(addr.wrapping_add(1), (val >> 8) as u8);
    }

    // --- I/Oポート空間 ---
    // Tier 2a でここをPIC(0x20,0xA0)/PIT(0x40-43)/UART(0x3F8)への
    // 振り分けに置き換える。今は素通しのバイト配列。

    pub fn io_read8(&mut self, port: u16) -> u8 {
        self.ports[port as usize]
    }

    pub fn io_write8(&mut self, port: u16, val: u8) {
        self.ports[port as usize] = val;
    }

    /// 16bitのI/Oは連続する2ポートへのアクセスとして扱う
    pub fn io_read16(&mut self, port: u16) -> u16 {
        self.io_read8(port) as u16 | (self.io_read8(port.wrapping_add(1)) as u16) << 8
    }

    pub fn io_write16(&mut self, port: u16, val: u16) {
        self.io_write8(port, val as u8);
        self.io_write8(port.wrapping_add(1), (val >> 8) as u8);
    }

    /// BIOS HLE: INT命令のフック。実IVTへのディスパッチは後段で実装する
    pub fn bios_interrupt(&mut self, n: u8) {
        match n {
            0x10 => {
                let ah = (self.cpu.regs[cpu::AX] >> 8) as u8;
                match ah {
                    0x0E => {
                        // テレタイプ出力: AL
                        self.console.push(self.cpu.regs[cpu::AX] as u8);
                    }
                    _ => panic!("INT 10h AH={ah:#04x} not implemented"),
                }
            }
            _ => panic!("INT {n:#04x} not implemented"),
        }
    }

    /// 1サイクル進める。
    ///
    /// 順序に意味がある。**割り込みの受付は命令の途中ではなく境界で行う**。
    /// 命令の実行中に割り込むと、書き換え途中のレジスタやスタックのまま
    /// ハンドラへ飛ぶことになり、`IRET` で戻っても再開できない。
    pub fn step(&mut self) {
        // 1. 保留中のハードウェア割り込みを受け付ける (IFが立っているときだけ)。
        //    HLTで止まっていてもここで目を覚ます — 割り込み待ちのHLTが
        //    成立するのはこのため
        //    `is_some()` を先に見るのは、`take()` が None を書き戻すため。
        //    毎命令の書き込みになり、実測で数%効いた (ベンチが捕まえた)
        if self.pending_irq.is_some() && self.cpu.flag(cpu::IF) {
            let vec = self.pending_irq.take().unwrap();
            self.halted = false;
            cpu::interrupt(self, vec);
            return;
        }
        if self.halted {
            return;
        }

        // 2. BIOS HLE の入口に居るなら、バイト列を実行せずホスト関数で肩代わりする。
        //    OSがIVTを書き換えていればここには来ない
        if self.cpu.sregs[cpu::CS] == BIOS_SEG {
            let vec = self.cpu.ip as u8;
            self.bios_interrupt(vec);
            cpu::iret(self);
            return;
        }

        // 3. 命令を実行する。TFは**実行前**の値を見る。
        //    ハンドラ内でTFが落ちても、この命令のシングルステップは成立させる
        let tf = self.cpu.flag(cpu::TF);
        cpu::step(self);

        // 4. トラップフラグが立っていたら、命令が終わってから INT 1。
        //    「実行してから止まる」ので、デバッガは1命令ずつ進められる
        if tf && !self.halted {
            cpu::interrupt(self, 1);
        }
    }

    /// HLTするか命令数上限まで実行。
    /// 割り込みが保留されていればHLTでも止まらない (目を覚ますため)
    pub fn run(&mut self, max_instructions: u64) -> u64 {
        let mut n = 0;
        while n < max_instructions {
            if self.halted && self.pending_irq.is_none() {
                break;
            }
            self.step();
            n += 1;
        }
        n
    }

    pub fn console_string(&self) -> String {
        String::from_utf8_lossy(&self.console).into_owned()
    }
}
