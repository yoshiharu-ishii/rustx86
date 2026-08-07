pub mod bus;
pub mod cpu;
pub mod dev;

pub use bus::{decode_io, decode_mem, Devices, IoTarget, MemRegion};
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

/// 何命令ごとに装置を進めるか。
///
/// 本来はCPUのクロックと装置のクロックを別々に数えるべきだが、このエミュレータは
/// サイクル数を持っていない。「1命令 ≒ 一定時間」と割り切って、まとめて進める。
///
/// ELKSやLinuxが要求するのは**周期割り込みが一定の間隔で来ること**であって、
/// 実時間との一致ではない。時計がずれても動作は壊れない。
pub const INSTRUCTIONS_PER_TICK: u32 = 64;

/// 1回の tick で装置に渡すクロック数。
///
/// 実測 (Tier 1c 時点) でおよそ 1億命令/秒 出ているので、
/// 1命令 ≒ 10ns → 64命令 ≒ 640ns。PITの入力は 1.193182 MHz (≒838ns周期) なので、
/// 64命令あたり 1クロックに近い。実時間に大きくは外れない
pub const PIT_CLOCKS_PER_TICK: u32 = 1;

/// IRQ0 (PIT) の割り込み線
pub const IRQ_TIMER: u8 = 0;
/// IRQ4 (COM1) の割り込み線
pub const IRQ_COM1: u8 = 4;

/// マシン全体。メモリとBIOS HLE (高位エミュレーション) を持つ。
/// 本物のBIOSは実装せず、INT命令をフックして最小限のサービスだけ提供する。
pub struct Machine {
    pub cpu: Cpu,
    pub mem: Vec<u8>,
    /// 保留中のハードウェア割り込みベクタ。IFが立っている命令境界で受け付ける。
    /// Tier 2a で 8259 PIC がここへ挙手する
    pub pending_irq: Option<u8>,
    /// I/Oポート空間にぶら下がる装置。中身は Tier 2b で実装する
    pub devices: Devices,
    /// 誰も名乗り出なかったポート番号。
    ///
    /// 実機は未接続のポートを読むと 0xFF が返るだけで、OSはこれを使って
    /// 装置の有無を探る。だから panic はできない。かといって黙って捨てると
    /// 「なぜ動かないのか」の手がかりが消えるので、触られた番号だけ覚えておく
    pub unhandled_io: std::collections::BTreeSet<u16>,
    /// テキストVRAMに書き込みがあったか。描画側が読んだら下ろす
    pub vram_dirty: bool,
    /// 装置を進めるまでの残り命令数。
    ///
    /// 装置を毎命令進めると、最も回数の多い経路に仕事が乗る。
    /// カウントダウン1本にしておけば、ほとんどの命令は「1減らして分岐」だけで済む
    tick_countdown: u32,
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
            devices: Devices::new(),
            unhandled_io: std::collections::BTreeSet::new(),
            vram_dirty: false,
            tick_countdown: INSTRUCTIONS_PER_TICK,
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

    /// ハードウェア割り込みベクタを直接立てる (PICを介さない経路。テスト用)
    pub fn raise_irq(&mut self, vector: u8) {
        self.pending_irq = Some(vector);
    }

    /// 装置を進め、挙手があればPICへ渡す。
    ///
    /// **一周はこうなっている**:
    /// PITがカウンタ0を下ろしきってIRQ0を出す → PICが優先順位を見て受理し、
    /// ICW2で設定されたベースから割り込みベクタを決める → CPUが命令境界で受け取る。
    /// この経路のどこか1つでも欠けるとOSのスケジューラが動かない。
    fn tick_devices(&mut self) {
        if self.devices.pit.tick(PIT_CLOCKS_PER_TICK) > 0 {
            self.devices.pic[0].raise(IRQ_TIMER);
        }
        if self.devices.uart.irq_pending {
            self.devices.pic[0].raise(IRQ_COM1);
        }
        if self.pending_irq.is_none() {
            self.pending_irq = self.devices.pic[0].acknowledge();
        }
    }

    pub fn read8(&self, addr: u32) -> u8 {
        self.mem[(addr as usize) & (MEM_SIZE - 1)]
    }

    /// メモリ書き込み。
    ///
    /// テキストVRAMは**メモリ空間に居座る装置**なので、素通しで `mem` に書く。
    /// 実機でもビデオカードのRAMがCPUのアドレス空間に窓として現れているだけで、
    /// 書き込み経路に特別な変換は無い。ここで足しているのは描画側への合図だけ。
    ///
    /// 読み出し ([`read8`](Self::read8)) には一切分岐を入れていない。
    /// メモリアクセスは最も回数の多い経路なので、**書き込み側だけで済む
    /// 仕掛けなら書き込み側に寄せる**。
    pub fn write8(&mut self, addr: u32, val: u8) {
        let a = (addr as usize) & (MEM_SIZE - 1);
        self.mem[a] = val;
        if (bus::VRAM_TEXT_BASE as usize..=bus::VRAM_TEXT_END as usize).contains(&a) {
            self.vram_dirty = true;
        }
    }

    pub fn read16(&self, addr: u32) -> u16 {
        self.read8(addr) as u16 | (self.read8(addr.wrapping_add(1)) as u16) << 8
    }

    pub fn write16(&mut self, addr: u32, val: u16) {
        self.write8(addr, val as u8);
        self.write8(addr.wrapping_add(1), (val >> 8) as u8);
    }

    // --- I/Oポート空間の振り分け ---

    /// ポートから読む。
    ///
    /// 未接続のポートは **0xFF** を返す。実機のISAバスは誰もドライブしないと
    /// プルアップで全ビットが立つためで、OSはこの値を見て「装置が居ない」と
    /// 判断する。ここで panic すると装置探索の段階で止まってしまう
    pub fn io_read8(&mut self, port: u16) -> u8 {
        match bus::decode_io(port) {
            IoTarget::Pic { slave } => {
                let p = &self.devices.pic[slave as usize];
                if port & 1 == 0 { p.read_command() } else { p.read_data() }
            }
            IoTarget::Pit => {
                let idx = (port & 3) as usize;
                if idx == 3 { 0xFF } else { self.devices.pit.read_counter(idx) }
            }
            IoTarget::Keyboard => self.devices.keyboard[usize::from(port == 0x64)],
            IoTarget::Uart => self.devices.uart.read(port & 7),
            IoTarget::Unmapped => {
                self.unhandled_io.insert(port);
                0xFF
            }
        }
    }

    pub fn io_write8(&mut self, port: u16, val: u8) {
        match bus::decode_io(port) {
            IoTarget::Pic { slave } => {
                let p = &mut self.devices.pic[slave as usize];
                if port & 1 == 0 { p.write_command(val) } else { p.write_data(val) }
            }
            IoTarget::Pit => {
                let idx = (port & 3) as usize;
                if idx == 3 {
                    self.devices.pit.write_control(val)
                } else {
                    self.devices.pit.write_counter(idx, val)
                }
            }
            IoTarget::Keyboard => self.devices.keyboard[usize::from(port == 0x64)] = val,
            IoTarget::Uart => self.devices.uart.write(port & 7, val),
            IoTarget::Unmapped => {
                self.unhandled_io.insert(port);
            }
        }
    }

    // --- テキストVRAM ---

    /// テキスト画面の生バイト列 (80×25、文字と属性が交互)
    pub fn text_vram(&self) -> &[u8] {
        let b = bus::VRAM_TEXT_BASE as usize;
        &self.mem[b..b + bus::TEXT_LEN]
    }

    /// 描画側が読んだ印。次の書き込みまで dirty が下りる
    pub fn take_vram_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.vram_dirty, false)
    }

    /// テキスト画面を文字列にする (属性を捨てて文字コードだけ拾う)。
    /// テストと確認用で、実際の描画は色も使う
    pub fn text_screen_string(&self) -> String {
        let v = self.text_vram();
        (0..bus::TEXT_ROWS)
            .map(|row| {
                let line: String = (0..bus::TEXT_COLS)
                    .map(|col| {
                        let c = v[(row * bus::TEXT_COLS + col) * bus::TEXT_CELL];
                        if (0x20..0x7F).contains(&c) { c as char } else { ' ' }
                    })
                    .collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string()
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
        // 2. 装置を進める。毎命令ではなくまとめて進め、ホットパスの負担を抑える
        self.tick_countdown -= 1;
        if self.tick_countdown == 0 {
            self.tick_countdown = INSTRUCTIONS_PER_TICK;
            self.tick_devices();
        }

        if self.halted {
            return;
        }

        // 3. BIOS HLE の入口に居るなら、バイト列を実行せずホスト関数で肩代わりする。
        //    OSがIVTを書き換えていればここには来ない
        if self.cpu.sregs[cpu::CS] == BIOS_SEG {
            let vec = self.cpu.ip as u8;
            self.bios_interrupt(vec);
            cpu::iret(self);
            return;
        }

        // 4. 命令を実行する。TFは**実行前**の値を見る。
        //    ハンドラ内でTFが落ちても、この命令のシングルステップは成立させる
        let tf = self.cpu.flag(cpu::TF);
        cpu::step(self);

        // 5. トラップフラグが立っていたら、命令が終わってから INT 1。
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
            // HLT中でも装置は動き続ける。タイマ割り込みで目を覚ますため、
            // 「保留が無ければ終わり」ではなく「タイマも止まっていれば終わり」で判定する
            if self.halted && self.pending_irq.is_none() && !self.devices.pit.counters[0].running {
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
