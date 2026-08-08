//! BIOS の高位エミュレーション (HLE)。
//!
//! **実機ではBIOSは別のROMである。** CPUや装置と同じ場所に置くと、
//! 「ハードウェアがやっていること」と「ファームウェアが肩代わりしていること」の
//! 区別が消える。ここに集めてあるのは全部**後者**で、本物のPCなら
//! マザーボード上のROMに焼かれている処理にあたる。
//!
//! 実BIOSは実装せず、必要なサービスだけホスト側の関数で肩代わりする
//! ([ADR-0001](../../../docs/adr/0001-16bit-cpu-and-cosim.md))。
//! やっていることは3つ:
//!
//! - **起動時の下ごしらえ** — IVT、BIOSデータエリア、PICの初期配置
//! - **INT で呼ばれるサービス** — 画面・ディスク・キーボード・時刻
//! - **ハードウェア割り込みの既定の受け皿** — OSが自分のハンドラを
//!   入れるまではBIOSが受ける
//!
//! OSがIVTを書き換えたベクタはここへ来なくなる。**乗っ取りに何の分岐も
//! 要らない**のがこの方式の要点である。

use crate::{bus, cpu, disk, Machine};

/// BIOS HLE の入口として予約したセグメント。
///
/// 起動時にIVTの全256エントリを `BIOS_SEG:n` で埋める。実行がここへ来たら
/// バイト列を解釈せずホスト側の関数で肩代わりし、`IRET` で戻る。
///
/// この形にしているのは、**OSがIVTを書き換えた瞬間に自然とHLEが外れる**ため。
/// OSが自分のハンドラを登録したベクタはもう `BIOS_SEG` を指していないので、
/// 何の分岐も足さずに乗っ取りが成立する。実機のBIOSとOSの関係そのものである。
pub const BIOS_SEG: u16 = 0xF000;

impl Machine {
    /// 電源投入時の下ごしらえ (POST: Power-On Self Test)。
    ///
    /// 実機のBIOSが、ブートセクタへ制御を渡す**前に**やっていること。
    /// これを飛ばすとOSは「マシンの仕様書も割り込みの宛先も無い世界」で
    /// 起動することになり、必ずどこかで転ぶ。実際にELKSは3回転んだ
    /// (A20が開かない / 画面の桁数が0 / タイマがゼロ除算として届く)。
    pub(crate) fn power_on_self_test(&mut self) {
        self.install_bios_vectors();
        self.install_bios_data_area();
        self.install_pic_defaults();
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
    /// この衝突は Tier 3 でもう一度顔を出す。
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

    /// BIOS HLE: 実BIOSは実装せず、必要なサービスだけホスト側の関数で肩代わりする。
    ///
    /// OSがIVTを書き換えたベクタはここへ来ない。**未実装のサービスは即panicする** —
    /// 静かに間違った値を返すと、遥か後方で意味不明な暴走として現れるためである。
    pub fn bios_interrupt(&mut self, n: u8) -> bool {
        let ah = (self.cpu.regs[cpu::AX] >> 8) as u8;
        match n {
            // --- INT 10h: ビデオ ---
            0x10 => match ah {
                0x00 => {} // ビデオモード設定 (テキストのみなので何もしない)
                0x01 => {} // カーソルの形 (描画側が決めているので覚えない)
                // AH=02: カーソルを動かす (DH=行 DL=桁)。
                // **DOSの画面はカーソル移動と書き込みの組で作られる。**
                // ELKSはVRAMを直接叩くのでここが空でも動いていた
                0x02 => {
                    let dx = self.cpu.regs[cpu::DX] as u16;
                    self.set_cursor_pos((dx >> 8) as usize, (dx & 0xFF) as usize);
                }
                // AH=03: カーソルの位置と形を返す
                0x03 => {
                    let (row, col) = self.cursor_pos();
                    self.cpu.regs[cpu::DX] = (row as u32) << 8 | col as u32;
                    self.cpu.regs[cpu::CX] = 0x0607; // カーソルの形 (BDAと同じ)
                }
                // AH=0E: テレタイプ出力。
                //
                // **実BIOSと同じくテキストVRAMへ書く。** 以前はデバッグ用の
                // 文字列へ積むだけだったので、BIOS越しに描くOS (DOS) の画面が
                // ブラウザに出なかった。ELKSがVRAMを直接叩くOSだったため、
                // この穴は今まで表に出ていなかった
                0x0E => self.teletype(self.cpu.regs[cpu::AX] as u8),
                // AH=05: 表示ページの切り替え (1ページしか無いので何もしない)
                0x05 => {}
                // AH=11: 文字ジェネレータ (フォント)。
                //
                // **このエミュレータはフォントを持っていない。**文字の絵はブラウザ側の
                // フォントで描いているので、ゲストがフォントを載せ替えても効かない。
                // AL=30 の「情報を教えろ」にだけ答え、載せ替えは黙って受ける。
                // DOSはここから**画面の行数**を知るので、返さないと画面計算が壊れる
                0x11 => {
                    if self.cpu.regs[cpu::AX] as u8 == 0x30 {
                        self.cpu.regs[cpu::CX] = 16; // 1文字あたりの走査線数
                        self.cpu.regs[cpu::DX] =
                            (self.cpu.regs[cpu::DX] & 0xFF00) | (bus::TEXT_ROWS as u32 - 1);
                        self.cpu.regs[cpu::BP] = 0; // ES:BP = フォントの在り処。持っていない
                        self.cpu.sregs[cpu::ES] = 0;
                    }
                }
                // AH=06/07: 画面の一部を上/下へずらす。
                // **DOSの画面はこれで動く** — COMMAND.COMの改行もクリアもここを通る
                0x06 | 0x07 => {
                    let lines = self.cpu.regs[cpu::AX] as u8;
                    let attr = (self.cpu.regs[cpu::BX] >> 8) as u8;
                    let cx = self.cpu.regs[cpu::CX] as u16;
                    let dx = self.cpu.regs[cpu::DX] as u16;
                    let (top, left) = ((cx >> 8) as usize, (cx & 0xFF) as usize);
                    let (bottom, right) = ((dx >> 8) as usize, (dx & 0xFF) as usize);
                    self.scroll_window(top, left, bottom, right, lines, attr, ah == 0x06);
                }
                // AH=08: カーソル位置の文字と属性を読む
                0x08 => {
                    let (row, col) = self.cursor_pos();
                    let a = bus::VRAM_TEXT_BASE + ((row * bus::TEXT_COLS + col) * 2) as u32;
                    let ch = self.read8(a) as u32;
                    let at = self.read8(a + 1) as u32;
                    self.cpu.regs[cpu::AX] = at << 8 | ch;
                }
                // AH=09/0A: カーソル位置に文字を書く (CX回繰り返す)。
                // 0x09 は属性も置き、0x0A は文字だけ置く
                0x09 | 0x0A => {
                    let ch = self.cpu.regs[cpu::AX] as u8;
                    let attr = (self.cpu.regs[cpu::BX] >> 8) as u8;
                    let count = (self.cpu.regs[cpu::CX] as u16).max(1) as usize;
                    let (row, col) = self.cursor_pos();
                    for i in 0..count {
                        let idx = row * bus::TEXT_COLS + col + i;
                        if idx >= bus::TEXT_COLS * bus::TEXT_ROWS {
                            break;
                        }
                        let a = bus::VRAM_TEXT_BASE + (idx * 2) as u32;
                        self.write8(a, ch);
                        if ah == 0x09 {
                            self.write8(a + 1, attr);
                        }
                    }
                }
                0x0F => {
                    // 現在のビデオモードを返す: AL=モード AH=桁数 BH=ページ
                    self.cpu.regs[cpu::AX] = 80 << 8 | 0x03;
                    self.cpu.regs[cpu::BX] &= 0x00FF;
                }
                // AH=13: 文字列をまとめて書く
                0x13 => {
                    let count = self.cpu.regs[cpu::CX] as u16 as usize;
                    let mode = self.cpu.regs[cpu::AX] as u8;
                    let attr = (self.cpu.regs[cpu::BX] >> 8) as u8;
                    let src = cpu::operand::linear(
                        self.cpu.sregs[cpu::ES],
                        self.cpu.regs[cpu::BP] as u16,
                    );
                    let dx = self.cpu.regs[cpu::DX] as u16;
                    let (mut row, mut col) = ((dx >> 8) as usize, (dx & 0xFF) as usize);
                    for i in 0..count {
                        // mode の bit1 が立っていると、文字のあとに属性が続く
                        let step = if mode & 2 != 0 { 2 } else { 1 };
                        let ch = self.read8(src + (i * step) as u32);
                        let at = if mode & 2 != 0 {
                            self.read8(src + (i * step) as u32 + 1)
                        } else {
                            attr
                        };
                        if ch == b'\n' {
                            row += 1;
                            col = 0;
                            continue;
                        }
                        if ch == b'\r' {
                            col = 0;
                            continue;
                        }
                        if row < bus::TEXT_ROWS && col < bus::TEXT_COLS {
                            let a = bus::VRAM_TEXT_BASE
                                + ((row * bus::TEXT_COLS + col) * 2) as u32;
                            self.write8(a, ch);
                            self.write8(a + 1, at);
                        }
                        col += 1;
                    }
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
            //
            // **DOSはここからキーを読む。** ELKSは8042を直接叩いてIRQ1で
            // 受けていたので、この入口が空でも動いていた。BIOS越しに触るOSでは
            // ここが本番になる。
            //
            // スキャンコードをASCIIに直すのは**BIOSの仕事**である。装置が返すのは
            // あくまでキーの位置で、文字にする対応表はファームウェアが持つ。
            0x16 => match ah {
                // 待って1つ取る。キーが無ければ**完了しない** — IRETせずに
                // 戻ることで、次のサイクルで同じINTがやり直される。
                // 実BIOSが STI + HLT で回っているのと同じ状態を作る
                0x00 | 0x10 => match self.take_key() {
                    Some(v) => self.cpu.regs[cpu::AX] = v as u32,
                    None => return false,
                },
                // 覗くだけ。無ければZF=1
                0x01 | 0x11 => match self.peek_key() {
                    Some(v) => {
                        self.cpu.regs[cpu::AX] = v as u32;
                        self.cpu.set_flag(cpu::ZF, false);
                    }
                    None => self.cpu.set_flag(cpu::ZF, true),
                },
                // シフト状態
                0x02 | 0x12 => self.cpu.regs[cpu::AX] = (self.cpu.regs[cpu::AX] & 0xFF00) | 0,
                // タイプマティック設定など。応答だけ返す
                0x03 | 0x05 => {}
                _ => panic!("INT 16h AH={ah:#04x} 未実装"),
            },

            // --- INT 1Ah: 時刻 ---
            //
            // AH=00 はBIOSが数えているティック、AH=02/04 はCMOSのRTCと、
            // **出どころが違う2つの時計**がある。起動時にRTCを一度読んで
            // ティックの側を合わせるのがDOSの流儀で、以後の時刻表示は
            // ティックの側から作られる。
            0x1A => match ah {
                // AH=00: 起動からのティック数 (CX:DX)。AL=日付が変わった回数
                0x00 => {
                    let ticks = self.read16(0x46C) as u32 | (self.read16(0x46E) as u32) << 16;
                    self.cpu.regs[cpu::CX] = ticks >> 16;
                    self.cpu.regs[cpu::DX] = ticks & 0xFFFF;
                    self.cpu.regs[cpu::AX] &= 0xFF00; // 日付跨ぎは数えていない
                }
                // AH=02: RTCから時刻を読む (CH=時 CL=分 DH=秒、いずれもBCD)
                0x02 => {
                    let (h, m, s) = self.devices.cmos.time_bcd();
                    self.cpu.regs[cpu::CX] = (h as u32) << 8 | m as u32;
                    self.cpu.regs[cpu::DX] = (s as u32) << 8; // DL=0: 夏時間ではない
                    self.cpu.set_flag(cpu::CF, false); // 電池は生きている
                }
                // AH=04: RTCから日付を読む (CH=世紀 CL=年 DH=月 DL=日、BCD)
                0x04 => {
                    let (c, y, mo, d) = self.devices.cmos.date_bcd();
                    self.cpu.regs[cpu::CX] = (c as u32) << 8 | y as u32;
                    self.cpu.regs[cpu::DX] = (mo as u32) << 8 | d as u32;
                    self.cpu.set_flag(cpu::CF, false);
                }
                // AH=01: ティック数を設定する。**DOSはRTCを読んでここへ書き戻す** —
                // 以後の時刻表示はティックの側から作られるので、この一手で
                // 2つの時計の辻褄が合う
                0x01 => {
                    self.write16(0x46C, self.cpu.regs[cpu::DX] as u16);
                    self.write16(0x46E, self.cpu.regs[cpu::CX] as u16);
                }
                // AH=03: RTCの時刻を設定する (DOSの `TIME`)
                0x03 => {
                    let cx = self.cpu.regs[cpu::CX];
                    let dx = self.cpu.regs[cpu::DX];
                    self.devices.cmos.set_time_bcd((cx >> 8) as u8, cx as u8, (dx >> 8) as u8);
                }
                // AH=05: RTCの日付を設定する (DOSの `DATE`)
                0x05 => {
                    let cx = self.cpu.regs[cpu::CX];
                    let dx = self.cpu.regs[cpu::DX];
                    self.devices
                        .cmos
                        .set_date_bcd((cx >> 8) as u8, cx as u8, (dx >> 8) as u8, dx as u8);
                }
                _ => panic!("INT 1Ah AH={ah:#04x} 未実装"),
            },

            _ => panic!(
                "INT {n:#04x} AH={ah:#04x} 未実装 (CS:IP={:04x}:{:04x})",
                self.cpu.sregs[cpu::CS],
                self.cpu.ip
            ),
        }
        true
    }

    /// 8042の待ち行列から1つ取り、`AH=スキャンコード AL=ASCII` に組む。
    /// 離した合図 (最上位ビット) と修飾キーはここで吸収する
    fn take_key(&mut self) -> Option<u16> {
        // 覗き見 (AH=01) で先に取り出してある分があればそれを返す。
        // 装置の待ち行列から一度出したものは、こちらで預かっている
        if let Some(v) = self.kbd_peeked.take() {
            return Some(v);
        }
        while self.devices.keyboard.has_data() {
            let sc = self.devices.keyboard.read_data();
            if sc == 0xFF || sc == 0xE0 {
                continue;
            }
            if sc & 0x80 != 0 {
                // 離した合図。Shiftなら状態を下ろす
                if sc & 0x7F == 0x2A || sc & 0x7F == 0x36 {
                    self.kbd_shift = false;
                }
                continue;
            }
            if sc == 0x2A || sc == 0x36 {
                self.kbd_shift = true;
                continue;
            }
            let ascii = scancode_to_ascii(sc, self.kbd_shift).unwrap_or(0);
            return Some((sc as u16) << 8 | ascii as u16);
        }
        None
    }

    /// 取らずに覗く。取ってしまった1つは控えておいて次で返す
    fn peek_key(&mut self) -> Option<u16> {
        if let Some(v) = self.kbd_peeked {
            return Some(v);
        }
        let v = self.take_key();
        self.kbd_peeked = v;
        v
    }

    /// テキスト画面の一部を上 (`up=true`) または下へずらす。
    /// `lines` が0なら範囲を空白で埋める (画面クリアはこの形で来る)
    /// テレタイプ出力1文字ぶん。カーソルを進め、右端で折り返し、
    /// 最下行を越えたら画面全体を1行上げる。**これがBIOSコンソールの本体**である
    fn teletype(&mut self, c: u8) {
        self.console.push(c); // 診断用の写し (CLIで起動ログを読むため)
        let (mut row, mut col) = self.cursor_pos();
        match c {
            b'\r' => col = 0,
            b'\n' => row += 1,
            0x08 => col = col.saturating_sub(1), // バックスペースは消さずに戻るだけ
            0x07 => {}                           // ベル。音はまだ鳴らさない (箱B3)
            _ => {
                let addr =
                    bus::VRAM_TEXT_BASE + ((row * bus::TEXT_COLS + col) * 2) as u32;
                self.write8(addr, c);
                // 属性はページ0の既定 (BLで指定されるのはグラフィックモードだけ)
                if self.read8(addr + 1) == 0 {
                    self.write8(addr + 1, 0x07);
                }
                col += 1;
            }
        }
        if col >= bus::TEXT_COLS {
            col = 0;
            row += 1;
        }
        if row >= bus::TEXT_ROWS {
            // 1行上げて最下行を空ける。実BIOSも同じことをしている
            self.scroll_window(0, 0, bus::TEXT_ROWS - 1, bus::TEXT_COLS - 1, 1, 0x07, true);
            row = bus::TEXT_ROWS - 1;
        }
        self.set_cursor_pos(row, col);
    }

    fn scroll_window(
        &mut self,
        top: usize,
        left: usize,
        bottom: usize,
        right: usize,
        lines: u8,
        attr: u8,
        up: bool,
    ) {
        let bottom = bottom.min(bus::TEXT_ROWS - 1);
        let right = right.min(bus::TEXT_COLS - 1);
        if top > bottom || left > right {
            return;
        }
        let cell = |r: usize, c: usize| bus::VRAM_TEXT_BASE + ((r * bus::TEXT_COLS + c) * 2) as u32;
        let n = if lines == 0 { bottom - top + 1 } else { lines as usize };

        for _ in 0..n.min(bottom - top + 1) {
            if lines != 0 {
                if up {
                    for r in top..bottom {
                        for c in left..=right {
                            let v = self.read8(cell(r + 1, c));
                            let a = self.read8(cell(r + 1, c) + 1);
                            self.write8(cell(r, c), v);
                            self.write8(cell(r, c) + 1, a);
                        }
                    }
                } else {
                    for r in (top + 1..=bottom).rev() {
                        for c in left..=right {
                            let v = self.read8(cell(r - 1, c));
                            let a = self.read8(cell(r - 1, c) + 1);
                            self.write8(cell(r, c), v);
                            self.write8(cell(r, c) + 1, a);
                        }
                    }
                }
            }
            let blank = if up { bottom } else { top };
            for c in left..=right {
                self.write8(cell(blank, c), b' ');
                self.write8(cell(blank, c) + 1, attr);
            }
            if lines == 0 {
                // 全消し: 残りの行も空白にする
                for r in top..=bottom {
                    for c in left..=right {
                        self.write8(cell(r, c), b' ');
                        self.write8(cell(r, c) + 1, attr);
                    }
                }
                break;
            }
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
            // AH=41: LBA拡張 (EDD) があるか。
            //
            // **無いと正直に答える。** EDDはハードディスクのための拡張で、
            // フロッピーには存在しない。CF=1 を返せばゲストはCHSへ引き返す。
            // ここで嘘をつくと、以後 AH=42 のパケット形式で読みに来られて詰む。
            // Tier 6c でCD-ROMをやるときに初めて「ある」と答えることになる
            0x41 => self.disk_error(0x01), // 機能が無効
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
}

/// スキャンコード → ASCII (US配列)。**BIOSの仕事**である。
///
/// 装置が返すのはキーの位置で、文字にする対応表はファームウェアが持つ。
/// 配列を差し替えられるのはこの層があるからで、
/// [`crate::dev::kbd::scancode_shift`] のちょうど逆向きにあたる。
fn scancode_to_ascii(sc: u8, shift: bool) -> Option<u8> {
    const PLAIN: &[(u8, u8)] = &[
        (0x02, b'1'), (0x03, b'2'), (0x04, b'3'), (0x05, b'4'), (0x06, b'5'),
        (0x07, b'6'), (0x08, b'7'), (0x09, b'8'), (0x0A, b'9'), (0x0B, b'0'),
        (0x0C, b'-'), (0x0D, b'='), (0x1A, b'['), (0x1B, b']'), (0x27, b';'),
        (0x28, b'\''), (0x29, b'`'), (0x2B, b'\\'), (0x33, b','), (0x34, b'.'),
        (0x35, b'/'),
    ];
    const SHIFTED: &[(u8, u8)] = &[
        (0x02, b'!'), (0x03, b'@'), (0x04, b'#'), (0x05, b'$'), (0x06, b'%'),
        (0x07, b'^'), (0x08, b'&'), (0x09, b'*'), (0x0A, b'('), (0x0B, b')'),
        (0x0C, b'_'), (0x0D, b'+'), (0x1A, b'{'), (0x1B, b'}'), (0x27, b':'),
        (0x28, b'"'), (0x29, b'~'), (0x2B, b'|'), (0x33, b'<'), (0x34, b'>'),
        (0x35, b'?'),
    ];
    const ROW_Q: &[u8] = b"qwertyuiop";
    const ROW_A: &[u8] = b"asdfghjkl";
    const ROW_Z: &[u8] = b"zxcvbnm";

    let letter = |base: u8, row: &[u8]| -> Option<u8> {
        let i = sc.checked_sub(base)? as usize;
        let c = *row.get(i)?;
        Some(if shift { c.to_ascii_uppercase() } else { c })
    };
    if (0x10..0x1A).contains(&sc) {
        return letter(0x10, ROW_Q);
    }
    if (0x1E..0x27).contains(&sc) {
        return letter(0x1E, ROW_A);
    }
    if (0x2C..0x33).contains(&sc) {
        return letter(0x2C, ROW_Z);
    }
    let table = if shift { SHIFTED } else { PLAIN };
    if let Some((_, c)) = table.iter().find(|(k, _)| *k == sc) {
        return Some(*c);
    }
    Some(match sc {
        0x1C => b'\r',
        0x0E => 8,
        0x0F => b'\t',
        0x01 => 27,
        0x39 => b' ',
        _ => return None,
    })
}
