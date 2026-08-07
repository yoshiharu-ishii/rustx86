pub mod cpu;

pub use cpu::Cpu;

pub const MEM_SIZE: usize = 1 << 20; // リアルモード 1MB

/// マシン全体。メモリとBIOS HLE (高位エミュレーション) を持つ。
/// 本物のBIOSは実装せず、INT命令をフックして最小限のサービスだけ提供する。
pub struct Machine {
    pub cpu: Cpu,
    pub mem: Vec<u8>,
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
        self.mem[0x7C00..0x7E00].copy_from_slice(sector);
        self.cpu.set_cs_ip(0x0000, 0x7C00);
        self.cpu.regs[cpu::DX] = 0x0080; // DL = ブートドライブ番号
        Ok(())
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

    /// 1命令実行
    pub fn step(&mut self) {
        if self.halted {
            return;
        }
        cpu::step(self);
    }

    /// HLTするか命令数上限まで実行
    pub fn run(&mut self, max_instructions: u64) -> u64 {
        let mut n = 0;
        while !self.halted && n < max_instructions {
            self.step();
            n += 1;
        }
        n
    }

    pub fn console_string(&self) -> String {
        String::from_utf8_lossy(&self.console).into_owned()
    }
}
