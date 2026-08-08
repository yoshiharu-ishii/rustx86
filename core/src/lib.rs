pub mod bios;
pub mod bus;
pub mod cp437;
pub mod cpu;
pub mod dev;
pub mod snapshot;
pub mod disk;

pub use bus::{decode_io, decode_mem, Devices, IoTarget, MemRegion};
pub use bios::BIOS_SEG;
pub use cpu::Cpu;
pub use disk::Disk;

pub const MEM_SIZE: usize = 1 << 20; // リアルモード 1MB


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
/// IRQ1 (キーボード) の割り込み線
pub const IRQ_KEYBOARD: u8 = 1;
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
    /// `0x66` (オペランドサイズ) を付けて実行されたオペコード。
    ///
    /// **幅対応を忘れた命令は静かに壊れる。** 即値や退避の長さが変わるので、
    /// 16bitのまま実行するとIPがずれ、以後はデータを命令として食い始める。
    /// panicも出ないまま遠くで暴走するので、**来たものを控えておく**。
    pub prefixed_ops: std::collections::BTreeSet<u8>,
    /// ゲストが設定しようとしたビデオモード。
    ///
    /// **テキスト以外は黙って無視している**ので、グラフィックスを要求された
    /// ことに気づけない。画面が真っ白なのが「何も描いていない」のか
    /// 「描いた先が無い」のかを区別するために控えておく
    pub video_modes: std::collections::BTreeSet<u8>,
    /// 装置を進めるまでの残り命令数。
    ///
    /// 装置を毎命令進めると、最も回数の多い経路に仕事が乗る。
    /// カウントダウン1本にしておけば、ほとんどの命令は「1減らして分岐」だけで済む
    tick_countdown: u32,
    /// INT 10h テレタイプ出力の蓄積 (画面代わり)
    pub console: Vec<u8>,
    /// ブートしたディスク。INT 13h のHLEが読む
    pub disk: Option<Disk>,
    /// BIOS が覚えているShiftの状態 (INT 16h の変換に使う)
    pub(crate) kbd_shift: bool,
    /// INT 16h AH=01 で覗いたが、まだ取られていないキー
    pub(crate) kbd_peeked: Option<u16>,
    /// 最初に起きたCPU例外の (ベクタ番号, CS, IP)。
    /// 実OSを動かすと「どこで壊れたか」だけが手がかりになるので控えておく
    pub first_fault: Option<(u8, u16, u16)>,
    /// ベクタごとの発生回数。全部数える (周期割り込みで溢れないよう回数だけ)
    pub int_counts: Vec<u32>,
    /// ベクタごとの初出位置 (CS, IP)
    pub int_first: Vec<(u16, u16)>,
    /// 直近の割り込み (ベクタ, CS, IP)。**panic直前に何が起きたかはここに出る**
    pub int_recent: std::collections::VecDeque<(u8, u16, u16)>,
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
            prefixed_ops: std::collections::BTreeSet::new(),
            video_modes: std::collections::BTreeSet::new(),
            tick_countdown: INSTRUCTIONS_PER_TICK,
            console: Vec::new(),
            disk: None,
            kbd_shift: false,
            kbd_peeked: None,
            first_fault: None,
            int_counts: vec![0; 256],
            int_first: vec![(0, 0); 256],
            int_recent: std::collections::VecDeque::with_capacity(33),
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
        // 時計もPITと同じクロックで進める。**ここで進めるのが要点**で、
        // INT 08h の中で進めるとOSが自前のハンドラを入れた瞬間に時計が止まる
        self.devices.cmos.tick(PIT_CLOCKS_PER_TICK);
        if self.devices.uart.irq_pending {
            self.devices.pic[0].raise(IRQ_COM1);
        }
        // キーボードは割り込み駆動。**1バイトにつき1回だけ**挙手する
        if self.devices.keyboard.take_irq() {
            self.devices.pic[0].raise(IRQ_KEYBOARD);
        }
        if self.pending_irq.is_none() {
            self.pending_irq = self.devices.pic[0].acknowledge();
        }
    }

    /// ディスクイメージを入れ、その先頭セクタからブートする
    pub fn boot_from_disk(&mut self, image: Vec<u8>) -> Result<(), String> {
        let d = Disk::from_image(image)?;
        let boot = d.read_sector(0).ok_or("ブートセクタが読めない")?.to_vec();
        self.disk = Some(d);
        self.power_on_self_test();
        self.mem[0x7C00..0x7E00].copy_from_slice(&boot);
        self.cpu.set_cs_ip(0x0000, 0x7C00);
        self.cpu.regs[cpu::DX] = 0x0000; // DL = 0 (フロッピーA)
        Ok(())
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

    pub fn read32(&self, addr: u32) -> u32 {
        self.read16(addr) as u32 | (self.read16(addr.wrapping_add(2)) as u32) << 16
    }

    pub fn write32(&mut self, addr: u32, val: u32) {
        self.write16(addr, val as u16);
        self.write16(addr.wrapping_add(2), (val >> 16) as u16);
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
            IoTarget::Keyboard => {
                if port == 0x64 {
                    self.devices.keyboard.read_status()
                } else {
                    self.devices.keyboard.read_data()
                }
            }
            IoTarget::Uart => self.devices.uart.read(port & 7),
            IoTarget::Cmos => {
                if port == 0x71 { self.devices.cmos.read_data() } else { 0xFF }
            }
            IoTarget::Crtc => {
                if port == 0x3D5 { self.devices.crtc.read_data() } else { 0xFF }
            }
            IoTarget::SystemControl => {
                // bit4 をトグルし続ける。OSがリフレッシュ矩形波を数えて
                // 時間を測る古い手口に付き合うため
                self.devices.sysctl ^= 0x10;
                self.devices.sysctl
            }
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
            IoTarget::SystemControl => self.devices.sysctl = val,
            IoTarget::Unmapped => {
                self.unhandled_io.insert(port);
            }
        }
    }

    // --- テキストVRAM ---

    /// テキスト画面の生バイト列 (80×25、文字と属性が交互)。
    ///
    /// **先頭から4000バイトではなく、CRTCが指す位置から4000バイトを返す。**
    ///
    /// テキストVRAMの窓は32KBあり、80x25の1画面はそのうち4000バイトでしかない。
    /// どこから表示するかを決めるのはCRTCのレジスタ 0x0C/0x0D で、ここを動かすと
    /// **メモリを1バイトも書き換えずに画面をスクロールできる** (ハードウェアスクロール)。
    /// 80年代の機械が遅いCPUで滑らかにスクロールできたのはこの仕組みによる。
    ///
    /// これを見ずに常に先頭を返していたため、CGA向けにハードウェアスクロールで
    /// 描くソフト (zmiy など) は**画面の下が永久に出てこなかった**。
    /// CRTCは実装してあり、説明にも「ここを動かすとスクロールできる」と
    /// 書いてあったのに、**描く側が見ていなかった**。
    pub fn text_vram(&self) -> &[u8] {
        let win = (bus::VRAM_TEXT_END - bus::VRAM_TEXT_BASE + 1) as usize;
        // 開始位置は文字単位。1文字2バイトなので倍にする
        let start = (self.devices.crtc.start_offset() as usize * bus::TEXT_CELL) % win;
        let b = bus::VRAM_TEXT_BASE as usize + start;
        // 窓の端をまたぐ場合は、素直に先頭を返す (実機は巻き戻るが、
        // そこまで使うソフトは見ていない。使うものが出てきたら組み立てる)
        if start + bus::TEXT_LEN <= win {
            &self.mem[b..b + bus::TEXT_LEN]
        } else {
            let base = bus::VRAM_TEXT_BASE as usize;
            &self.mem[base..base + bus::TEXT_LEN]
        }
    }

    /// 機械の状態をまるごと書き出す。
    ///
    /// **CPUだけでは足りない。** PICのマスクが失われれば以後の割り込みが
    /// 来なくなり、PITのカウンタが戻れば時計が飛ぶ。装置もメモリも
    /// ディスクも含めて初めて「あの瞬間から再開」ができる。
    pub fn save_state(&self) -> Vec<u8> {
        let mut w = snapshot::Writer::new();
        snapshot::write_header(&mut w);

        // CPU
        for r in self.cpu.regs {
            w.u32(r);
        }
        for s in self.cpu.sregs {
            w.u16(s);
        }
        w.u16(self.cpu.ip);
        w.u32(self.cpu.flags);

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
        let mut r = snapshot::Reader::new(data);
        snapshot::read_header(&mut r)?;

        for i in 0..8 {
            m.cpu.regs[i] = r.u32()?;
        }
        for i in 0..6 {
            m.cpu.sregs[i] = r.u16()?;
        }
        m.cpu.ip = r.u16()?;
        m.cpu.flags = r.u32()?;

        m.halted = r.bool()?;
        m.pending_irq = r.opt_u8()?;

        for i in 0..2 {
            m.devices.pic[i].load(&mut r)?;
        }
        m.devices.pit.load(&mut r)?;
        m.devices.uart.load(&mut r)?;
        m.devices.keyboard.load(&mut r)?;
        m.devices.cmos.load(&mut r)?;
        m.devices.crtc.load(&mut r)?;

        let mem = r.rle()?;
        if mem.len() != MEM_SIZE {
            return Err(format!("メモリの大きさが合わない ({} != {MEM_SIZE})", mem.len()));
        }
        m.mem = mem;
        m.disk = if r.bool()? {
            Some(Disk::from_image(r.rle()?)?)
        } else {
            None
        };

        *self = m;
        Ok(())
    }

    /// BIOSサービスが返した成否をフラグとして呼び出し元へ届ける。
    ///
    /// **`IRET` はスタックに積まれたFLAGSで上書きしてしまう。** サービスが
    /// `CF` や `ZF` を立てても、そのまま戻ると消える。実BIOSも同じ事情を
    /// 抱えていて、**積まれている方のFLAGSを書き換えてから**戻る。
    ///
    /// x86のBIOSが慣例として「成否はキャリーフラグで返す」形なのに、
    /// この一手間が要るのは面白いところである。
    fn return_flags_to_caller(&mut self) {
        // スタック: [SP]=IP [SP+2]=CS [SP+4]=FLAGS
        let sp = self.cpu.regs[cpu::SP] as u16;
        let addr = cpu::operand::linear(self.cpu.sregs[cpu::SS], sp.wrapping_add(4));
        let stacked = self.read16(addr);
        let keep = (cpu::CF | cpu::ZF) as u16;
        self.write16(addr, (stacked & !keep) | (self.cpu.flags as u16 & keep));
    }

    /// カーソルの位置 (行, 桁)。CRTCが持っている
    pub fn cursor_pos(&self) -> (usize, usize) {
        let off = self.devices.crtc.cursor_offset() as usize;
        (off / bus::TEXT_COLS, off % bus::TEXT_COLS)
    }

    /// カーソルを (行, 桁) へ動かす。
    ///
    /// **CRTCとBIOSデータエリアの両方に書く。** 実機でもこの2箇所に同じ位置が
    /// 載っていて、画面はCRTCを見るが、**ソフトはBDAの方を直接読むことがある**。
    /// BDA側 (0x450、ページ0のカーソル位置) を更新していなかったので、
    /// 覗いた側からはいつまでも「行0桁0」に見えていた。
    pub fn set_cursor_pos(&mut self, row: usize, col: usize) {
        let row = row.min(bus::TEXT_ROWS - 1);
        let col = col.min(bus::TEXT_COLS - 1);
        self.devices
            .crtc
            .set_cursor_offset((row * bus::TEXT_COLS + col) as u16);
        self.write16(0x450, (row as u16) << 8 | col as u16);
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
                    .map(|col| cp437::to_char(v[(row * bus::TEXT_COLS + col) * bus::TEXT_CELL]))
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
            // 完了しなかったサービス (キー待ちなど) はIRETせずに戻る。
            // 次のサイクルで同じINTがやり直され、実BIOSが割り込みを待って
            // 回っているのと同じ状態になる
            if self.bios_interrupt(vec) {
                self.return_flags_to_caller();
                cpu::iret(self);
            }
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
