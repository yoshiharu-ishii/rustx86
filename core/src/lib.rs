pub mod bus;
pub mod cpu;
pub mod dev;
pub mod disk;

pub use bus::{decode_io, decode_mem, Devices, IoTarget, MemRegion};
pub use cpu::Cpu;
pub use disk::Disk;

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
    /// 装置を進めるまでの残り命令数。
    ///
    /// 装置を毎命令進めると、最も回数の多い経路に仕事が乗る。
    /// カウントダウン1本にしておけば、ほとんどの命令は「1減らして分岐」だけで済む
    tick_countdown: u32,
    /// INT 10h テレタイプ出力の蓄積 (画面代わり)
    pub console: Vec<u8>,
    /// ブートしたディスク。INT 13h のHLEが読む
    pub disk: Option<Disk>,
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
            tick_countdown: INSTRUCTIONS_PER_TICK,
            console: Vec::new(),
            disk: None,
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
        self.install_bios_vectors();
        self.install_bios_data_area();
        self.install_pic_defaults();
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

    /// BIOSデータエリア (0x400-0x4FF) を作る。
    ///
    /// 実BIOSが起動時に埋める「マシンの仕様書」で、**OSはここを読んで
    /// 画面の大きさやポート番号を知る**。IVTと同じく単なるメモリなので、
    /// 我々も同じ場所に同じ形で置けばOSからは区別がつかない。
    ///
    /// ここが空だとELKSのコンソールドライバが桁数0の画面に書こうとして
    /// 何も出なくなる (実際にそれで詰まった)。
    fn install_bios_data_area(&mut self) {
        self.write16(0x400, 0x3F8); // COM1 のポート番号
        self.write16(0x408, 0x378); // LPT1
        // 装置構成: フロッピー1台 + 80x25カラー + シリアル1本
        self.write16(0x410, 0x0021 | (1 << 9));
        self.write16(0x413, 640); // コンベンショナルメモリ (KB)
        self.write8(0x449, 0x03); // ビデオモード 3 = 80x25 カラーテキスト
        self.write16(0x44A, 80); // 桁数
        self.write16(0x44C, 0x1000); // 1ページのバイト数
        self.write16(0x44E, 0x0000); // 表示中ページの先頭オフセット
        for page in 0..8u32 {
            self.write16(0x450 + page * 2, 0); // 各ページのカーソル位置
        }
        self.write16(0x460, 0x0607); // カーソルの形
        self.write8(0x462, 0); // 表示中のページ番号
        self.write16(0x463, 0x3D4); // CRTC のポート番号 (カラー)
        self.write8(0x475, 0); // ハードディスクの台数
        self.write8(0x484, 24); // 行数 - 1
    }

    /// PICを実BIOSと同じ配置で初期化する。
    ///
    /// **これを忘れると悲惨なことになる。** ICW2で決めるベクタのベースが0のままだと、
    /// タイマのIRQ0が「ベクタ0 = ゼロ除算例外」として配送される。OSから見れば
    /// 突然デタラメな場所でゼロ除算が起きたことになり、`panic: DIVIDE FAULT` で死ぬ。
    /// 実際にELKSがこれで落ちた。
    ///
    /// OSはBIOSが初期化済みであることを前提に、マスクを緩めるだけのことが多い。
    /// マスタをベクタ 0x08-0x0F、スレーブを 0x70-0x77 に置くのがPC/ATの決まりである。
    ///
    /// なお 0x08 はプロテクトモードではCPUの例外番号 (#DF) と衝突する。
    /// Linuxが起動時にわざわざ 0x20 へ付け替えるのはこのためで、
    /// この衝突は Tier 4 でもう一度顔を出す。
    fn install_pic_defaults(&mut self) {
        for (i, base, icw3) in [(0usize, 0x08u8, 0x04u8), (1, 0x70, 0x02)] {
            let p = &mut self.devices.pic[i];
            p.write_command(0x11); // ICW1: 初期化開始 + ICW4あり
            p.write_data(base); // ICW2: ベクタのベース
            p.write_data(icw3); // ICW3: カスケードの結線
            p.write_data(0x01); // ICW4: 8086モード
            p.write_data(0xFF); // 全マスク。OSが必要な線だけ開ける
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
        self.install_bios_vectors();
        self.install_bios_data_area();
        self.install_pic_defaults();
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

    /// テキスト画面の生バイト列 (80×25、文字と属性が交互)
    pub fn text_vram(&self) -> &[u8] {
        let b = bus::VRAM_TEXT_BASE as usize;
        &self.mem[b..b + bus::TEXT_LEN]
    }

    /// カーソルの位置 (行, 桁)。CRTCが持っている
    pub fn cursor_pos(&self) -> (usize, usize) {
        let off = self.devices.crtc.cursor_offset() as usize;
        (off / bus::TEXT_COLS, off % bus::TEXT_COLS)
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

    /// BIOS HLE: 実BIOSは実装せず、必要なサービスだけホスト側の関数で肩代わりする。
    ///
    /// OSがIVTを書き換えたベクタはここへ来ない。**未実装のサービスは即panicする** —
    /// 静かに間違った値を返すと、遥か後方で意味不明な暴走として現れるためである。
    pub fn bios_interrupt(&mut self, n: u8) {
        let ah = (self.cpu.regs[cpu::AX] >> 8) as u8;
        match n {
            // --- INT 10h: ビデオ ---
            0x10 => match ah {
                0x00 => {} // ビデオモード設定 (テキストのみなので何もしない)
                0x01 | 0x02 | 0x03 => {} // カーソル形状・位置
                0x0E => {
                    // テレタイプ出力: AL
                    let c = self.cpu.regs[cpu::AX] as u8;
                    self.console.push(c);
                }
                0x0F => {
                    // 現在のビデオモードを返す: AL=モード AH=桁数 BH=ページ
                    self.cpu.regs[cpu::AX] = 80 << 8 | 0x03;
                    self.cpu.regs[cpu::BX] &= 0x00FF;
                }
                _ => panic!("INT 10h AH={ah:#04x} 未実装"),
            },

            // --- INT 08h: タイマ割り込み (IRQ0) ---
            // OSが自前のハンドラを入れるまではBIOSが受ける。実BIOSは
            // BDAのティックカウンタを進め、INT 1Ch (利用者用フック) を呼び、
            // PICにEOIを打つ。**EOIを忘れると以後の割り込みが二度と来なくなる**
            0x08 => {
                let ticks = self.read16(0x46C) as u32 | (self.read16(0x46E) as u32) << 16;
                let next = ticks.wrapping_add(1);
                self.write16(0x46C, next as u16);
                self.write16(0x46E, (next >> 16) as u16);
                self.devices.pic[0].write_command(0x20); // 非特定EOI
            }

            // --- INT 09h: キーボード割り込み (IRQ1) ---
            0x09 => {
                let _ = self.io_read8(0x60); // スキャンコードを捨てる
                self.devices.pic[0].write_command(0x20);
            }

            // --- INT 0Ah-0Fh / 70h-77h: ハンドラ未登録のハードウェア割り込み ---
            // 実BIOSも「EOIを打って帰るだけ」のスタブを置いている。
            // ここで落とすと、装置が1つ挙手しただけでマシンが死ぬ
            0x0A..=0x0F => self.devices.pic[0].write_command(0x20),
            0x70..=0x77 => {
                self.devices.pic[1].write_command(0x20);
                self.devices.pic[0].write_command(0x20); // カスケード元にも必要
            }

            // --- INT 1Ch: 利用者用タイマフック。既定は何もしない ---
            0x1C => {}

            // --- INT 11h: 装置構成 ---
            0x11 => self.cpu.regs[cpu::AX] = 0x0021, // フロッピー1台 + 80x25カラー

            // --- INT 12h: コンベンショナルメモリの大きさ (KB) ---
            // ELKSのブートセクタはこれを1命令目で呼び、返り値から自分の
            // 移動先セグメントを計算する
            0x12 => self.cpu.regs[cpu::AX] = 640,

            // --- INT 13h: ディスク ---
            0x13 => self.bios_disk(ah),

            // --- INT 15h: システムサービス ---
            0x15 => {
                // 未対応の機能は「サポートしていない」と答える。
                // OSは戻り値を見て別の手段へ回るので、ここで落としてはいけない
                self.cpu.set_flag_cf(true);
                self.cpu.regs[cpu::AX] = (self.cpu.regs[cpu::AX] & 0x00FF) | 0x8600;
            }

            // --- INT 16h: キーボード ---
            0x16 => match ah {
                0x00 | 0x10 => self.cpu.regs[cpu::AX] = 0, // 入力なし
                0x01 | 0x11 => self.cpu.set_flag(cpu::ZF, true), // バッファ空
                _ => panic!("INT 16h AH={ah:#04x} 未実装"),
            },

            // --- INT 1Ah: 時刻 ---
            0x1A => match ah {
                0x00 => {
                    self.cpu.regs[cpu::CX] = 0;
                    self.cpu.regs[cpu::DX] = 0;
                    self.cpu.regs[cpu::AX] &= 0xFF00;
                }
                _ => panic!("INT 1Ah AH={ah:#04x} 未実装"),
            },

            _ => panic!(
                "INT {n:#04x} AH={ah:#04x} 未実装 (CS:IP={:04x}:{:04x})",
                self.cpu.sregs[cpu::CS],
                self.cpu.ip
            ),
        }
    }

    /// INT 13h。ディスクイメージの該当セクタをメモリへ写す
    fn bios_disk(&mut self, ah: u8) {
        let Some(disk) = &self.disk else {
            self.disk_error(0x80); // タイムアウト = ドライブ無し
            return;
        };
        match ah {
            // AH=00: リセット。何もせず成功
            0x00 => self.disk_ok(0),
            // AH=02: セクタ読み出し / AH=03: セクタ書き込み。
            // CHSの解き方と転送先の求め方は同じで、向きだけが違う
            0x02 | 0x03 => {
                let ax = self.cpu.regs[cpu::AX];
                let cx = self.cpu.regs[cpu::CX] as u16;
                let dx = self.cpu.regs[cpu::DX] as u16;
                let count = (ax & 0xFF) as usize;
                // CHSの詰め方: CH=シリンダ下位8bit、CLのbit6-7がシリンダ上位2bit、
                // CLのbit0-5がセクタ番号 (1始まり)。10bit分をひねって押し込んでいる
                let cyl = (cx >> 8) | ((cx & 0xC0) << 2);
                let sec = (cx & 0x3F) as u8;
                let head = (dx >> 8) as u8;
                let Some(lba) = disk.chs_to_lba(cyl, head, sec) else {
                    self.disk_error(0x04); // セクタが見つからない
                    return;
                };
                let addr =
                    cpu::operand::linear(self.cpu.sregs[cpu::ES], self.cpu.regs[cpu::BX] as u16);
                if ah == 0x02 {
                    let mut buf = Vec::with_capacity(count * disk::SECTOR_SIZE);
                    for i in 0..count {
                        match disk.read_sector(lba + i) {
                            Some(s) => buf.extend_from_slice(s),
                            None => {
                                self.disk_error(0x04);
                                return;
                            }
                        }
                    }
                    for (i, b) in buf.iter().enumerate() {
                        self.write8(addr.wrapping_add(i as u32), *b);
                    }
                } else {
                    // 書き込みはイメージ上だけで、ファイルには反映しない。
                    // ルートファイルシステムをマウントするだけでもOSは書きに来るので、
                    // 応答しないと起動できない
                    let mut buf = vec![0u8; count * disk::SECTOR_SIZE];
                    for (i, b) in buf.iter_mut().enumerate() {
                        *b = self.read8(addr.wrapping_add(i as u32));
                    }
                    let d = self.disk.as_mut().unwrap();
                    for i in 0..count {
                        let s = &buf[i * disk::SECTOR_SIZE..(i + 1) * disk::SECTOR_SIZE];
                        if !d.write_sector(lba + i, s) {
                            self.disk_error(0x04);
                            return;
                        }
                    }
                }
                self.disk_ok(count as u8);
            }
            // AH=08: ドライブの形状を返す
            0x08 => {
                let (c, h, s) = (disk.cylinders, disk.heads, disk.sectors);
                self.cpu.regs[cpu::CX] =
                    ((((c - 1) & 0xFF) << 8) | (((c - 1) >> 2) & 0xC0) | s as u16) as u32;
                self.cpu.regs[cpu::DX] = (((h - 1) as u32) << 8) | 1; // DL = ドライブ台数
                self.cpu.regs[cpu::BX] = (self.cpu.regs[cpu::BX] & 0xFF00) | 0x04; // 1.44MB
                self.disk_ok(0);
            }
            // AH=15: ドライブの種類
            0x15 => {
                self.cpu.regs[cpu::AX] = (self.cpu.regs[cpu::AX] & 0x00FF) | 0x0100;
                self.cpu.set_flag_cf(false);
            }
            // AH=16: メディア交換の有無 / AH=17,18: フォーマット準備。
            // 「変わっていない」「対応している」と答えるだけでよい
            0x16 => self.disk_ok(0),
            0x17 | 0x18 => self.disk_ok(0),
            _ => panic!("INT 13h AH={ah:#04x} 未実装"),
        }
    }

    fn disk_ok(&mut self, sectors: u8) {
        self.cpu.regs[cpu::AX] = sectors as u32; // AH=0 (成功)
        self.cpu.set_flag_cf(false);
    }

    fn disk_error(&mut self, code: u8) {
        self.cpu.regs[cpu::AX] = (code as u32) << 8;
        self.cpu.set_flag_cf(true);
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
