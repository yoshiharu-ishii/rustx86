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

/// BIOSデータエリアの先頭 (セグメント 0x40)
const BDA_SEG: u32 = 0x400;
/// 修飾キーの状態 (bit0=右Shift bit1=左Shift bit2=Ctrl bit3=Alt)
const BDA_KB_FLAG1: u32 = 0x417;
/// 待ち行列の先頭位置 (セグメント0x40内のオフセット)
const BDA_KB_HEAD: u32 = 0x41A;
/// 待ち行列の末尾位置
const BDA_KB_TAIL: u32 = 0x41C;
/// 待ち行列の範囲。16個 (1個2バイト) 分
const KB_BUF_START: u16 = 0x1E;
const KB_BUF_END: u16 = 0x3E;

impl Machine {
    /// 電源投入時の下ごしらえ (POST: Power-On Self Test)。
    ///
    /// 実機のBIOSが、ブートセクタへ制御を渡す**前に**やっていること。
    /// これを飛ばすとOSは「マシンの仕様書も割り込みの宛先も無い世界」で
    /// 起動することになり、必ずどこかで転ぶ。実際にELKSは3回転んだ
    /// (A20が開かない / 画面の桁数が0 / タイマがゼロ除算として届く)。
    pub(crate) fn power_on_self_test(&mut self) {
        self.install_bios_rom_id();
        self.install_bios_vectors();
        self.install_bios_data_area();
        self.install_pic_defaults();
        self.install_pit_defaults();
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
                                    // キーボードの待ち行列。**空の状態は head == tail** で表す。
                                    // 位置を 0x480/0x482 にも書くのは、ここを見て別の場所に付け替える
                                    // プログラムがあるためである (常駐ソフトが行列を広げる手口)
        self.write16(BDA_KB_HEAD, KB_BUF_START);
        self.write16(BDA_KB_TAIL, KB_BUF_START);
        self.write16(0x480, KB_BUF_START);
        self.write16(0x482, KB_BUF_END);
        self.write8(0x475, 0); // ハードディスクの台数
        self.write8(0x484, 24); // 行数 - 1
    }

    /// ROMの末尾に**機種の名札**を置く。
    ///
    /// 実機のBIOS ROMには、末尾に日付と機種コードが焼かれている。
    /// ソフトはここを読んで「どの世代の機械か」を決め、その先の判断に使う。
    /// **置いていなかったので 0 が読まれ**、どの機種でもない機械に見えていた。
    ///
    /// - `F000:FFF5` — リリース日 "MM/DD/YY"
    /// - `F000:FFFE` — 機種コード。**0xFE = PC/XT**。
    ///   このマシンは8086 + CGA相当なので、それ以上を名乗らない
    fn install_bios_rom_id(&mut self) {
        const ROM: u32 = 0xF_0000;
        for (i, b) in b"08/08/26".iter().enumerate() {
            self.write8(ROM + 0xFFF5 + i as u32, *b);
        }
        self.write8(ROM + 0xFFFE, 0xFE); // PC/XT
        self.write8(ROM + 0xFFFF, 0x00); // 副機種コード
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
                                // **タイマ(0)・キーボード(1)・スレーブ連結(2)は開けておく。**
                                //
                                // 全部閉じたままにしていたところ、BIOSデータエリアの待ち行列に
                                // キーが一度も積まれなかった。INT 09h が呼ばれないためである。
                                // 実BIOSもここは開けて渡す — キーが押されたことを知る手段が
                                // 割り込みしか無いのだから、閉じたまま渡す意味が無い
            p.write_data(if i == 0 { 0xF8 } else { 0xFF });
        }
    }

    /// 実BIOSが起動時に行うPITの設定を再現する。
    ///
    /// **これを怠るとタイマ割り込みが一度も来ない。** ELKSは自分でPITを
    /// 設定するので気づかなかったが、**DOSはBIOSが設定済みであることを前提**に
    /// している。設定しないままだと、時計が進まないだけでなく
    /// 「HLTして割り込みを待つ」形の待ち合わせが**永久に目を覚まさない**。
    ///
    /// カウンタ0は分周値0 = 65536 で 18.2 Hz。この半端な数字は
    /// [`pit`](crate::dev::isa::pit) の説明のとおり、NTSCの水晶を流用した名残である。
    /// カウンタ1はDRAMリフレッシュ用で、出力は使わないが現在値を読んで
    /// 時間を測るプログラムがあるので動かしておく。
    fn install_pit_defaults(&mut self) {
        let pit = &mut self.devices.pit;
        pit.write_control(0x36); // カウンタ0、LoHi、モード3 (方形波)
        pit.write_counter(0, 0x00);
        pit.write_counter(0, 0x00); // 分周値0 = 65536 → 18.2 Hz
        pit.write_control(0x54); // カウンタ1、LoOnly、モード2 (レート生成)
        pit.write_counter(1, 18); // DRAMリフレッシュ
    }

    /// BIOS HLE: 実BIOSは実装せず、必要なサービスだけホスト側の関数で肩代わりする。
    ///
    /// OSがIVTを書き換えたベクタはここへ来ない。**未実装のサービスは即panicする** —
    /// 静かに間違った値を返すと、遥か後方で意味不明な暴走として現れるためである。
    pub fn bios_interrupt(&mut self, n: u8) -> bool {
        let ah = (self.cpu.regs[cpu::AX] >> 8) as u8;
        match n {
            // --- INT 10h: ビデオ ---
            0x10 => {
                if std::env::var("RUSTX86_TRACE_ALL").is_ok() {
                    eprintln!(
                        "INT10 AH={ah:#04x} AL={:#04x} BX={:#06x}",
                        self.cpu.regs[cpu::AX] as u8,
                        self.cpu.regs[cpu::BX]
                    );
                }
                match ah {
                    // AH=00: ビデオモード設定。**テキスト以外は実現できない**。
                    //
                    // 落とさずに受けるのは、モードを試して戻すプログラムがあるためだが、
                    // 黙って無視すると「描いた先が存在しない」ことに誰も気づけない。
                    // 要求されたモードを控えて、後から言えるようにしておく
                    0x00 => {
                        self.video_modes.insert(self.cpu.regs[cpu::AX] as u8 & 0x7F);
                    }
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
                    // AH=10: パレットレジスタの操作 (EGA/VGA)。
                    //
                    // **受けるが何も起きない。** 色は描画側 (ブラウザ) が固定の
                    // 16色で持っていて、ゲストが色番号の対応を差し替える先が無い。
                    // 落とさないのは、起動時に一度触るだけのプログラムが多いためである
                    0x10 => {}
                    // AH=12: 追加機能の選択 (EGA/VGA)。**対応していないと答える**
                    0x12 => {}
                    // AH=EF: Herculesグラフィックカードの探索。
                    // 標準のBIOSには無く、当時のライブラリが勝手に使った番号である。
                    // **載っていないので、そう答える**
                    0xEF => self.cpu.regs[cpu::DX] = 0xFFFF,
                    // AH=1A: 表示装置の種別を尋ねる (VGA BIOSから) /
                    // AH=1B: 機能と状態の一覧を尋ねる (VGA BIOSから)
                    //
                    // **どちらも「対応していない」と答える。** このマシンは
                    // MC6845のCRTCと 0xB8000 のテキストVRAMを持つ CGA 相当であって、
                    // VGAではない。呼び出し側は AL が合図の値になっているかで
                    // 対応の有無を判断するので、そのままにしておけば伝わる。
                    //
                    // `INT 13h AH=41` (LBA拡張) と同じで、**無いものを在ると
                    // 答えないこと**が肝心である。嘘をつくと、次にVGA前提の
                    // 手順で話しかけられて詰む
                    0x1A | 0x1B => {
                        if std::env::var("RUSTX86_TRACE_VIDEO").is_ok() {
                            // INTで積まれた戻り先を覗く: SS:SP に IP、+2 に CS
                            let sp = self.cpu.regs[cpu::SP] as u16;
                            let ss = self.cpu.sregs[cpu::SS];
                            let (rip, rcs) = (
                                self.read16(cpu::operand::linear(ss, sp)),
                                self.read16(cpu::operand::linear(ss, sp.wrapping_add(2))),
                            );
                            let base = cpu::operand::linear(rcs, rip);
                            let bytes: Vec<String> = (0..24)
                                .map(|i| format!("{:02x}", self.read8(base + i)))
                                .collect();
                            // 中継表 (INT n; RET) の1つ上 = 本当の呼び出し元。
                            // 近距離callなので戻り番地は2バイトで SS:SP+6 に積まれている
                            let up = self.read16(cpu::operand::linear(ss, sp.wrapping_add(6)));
                            let ubase = cpu::operand::linear(rcs, up);
                            let ub: Vec<String> = (0..32)
                                .map(|i| format!("{:02x}", self.read8(ubase + i)))
                                .collect();
                            eprintln!(
                            "AH={ah:#04x} 中継 {rcs:04x}:{rip:04x} [{}]\n  本当の呼び出し元 {rcs:04x}:{up:04x} 戻った直後: {}",
                            bytes[..6].join(" "), ub.join(" ")
                        );
                        }
                        self.cpu.regs[cpu::AX] &= 0xFF00; // AL≠0x1A = 「その機能は無い」
                                                          // **BXも埋める。**
                                                          //
                                                          // 作法どおりならALを見て「非対応」と分かるので BX は触らなくてよい。
                                                          // だが BL (装置の種別) だけを見るプログラムがあり、そういう相手には
                                                          // **前の呼び出しの残りかす**がそのまま種別として見えてしまう。
                                                          // 実際 zmiy がこれで「VGAだ」と判断し、80x50 で描いて画面から
                                                          // はみ出していた。BL=0x02 = カラーCGA、BH=0x00 = 副画面なし。
                        self.cpu.regs[cpu::BX] = 0x0002;
                    }
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
                                let a =
                                    bus::VRAM_TEXT_BASE + ((row * bus::TEXT_COLS + col) * 2) as u32;
                                self.write8(a, ch);
                                self.write8(a + 1, at);
                            }
                            col += 1;
                        }
                    }
                    _ => panic!("INT 10h AH={ah:#04x} 未実装"),
                }
            }

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
            //
            // **実BIOSの本体はここである。** 8042からスキャンコードを取り、
            // 修飾キーの状態を更新し、文字に直して**BIOSデータエリアの
            // 待ち行列へ積む**。INT 16h はその待ち行列から取り出すだけの薄い口で、
            // 装置には触らない。
            //
            // 以前はここでスキャンコードを捨て、INT 16h が8042を直接読んでいた。
            // それでも INT 16h しか使わないプログラムは動くが、
            // **BIOSデータエリアを直接覗くプログラム** (DOSには多い) からは
            // キーが永久に来ないように見える。実際それでFreeDOSのインストーラが
            // 入力待ちのまま止まっていた。
            0x09 => {
                self.keyboard_isr();
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
                    None => {
                        // **待つなら割り込みを開ける。**
                        //
                        // `INT` 命令はIFを落とす (x86の仕様)。落としたまま待つと
                        // キーボード割り込みが永久に来ず、待っているものが
                        // 二度と届かない。実BIOSの待ちループに `STI` があるのは
                        // このためで、ここでも同じことをする
                        self.cpu.set_flag(cpu::IF, true);
                        return false;
                    }
                },
                // 覗くだけ。無ければZF=1
                0x01 | 0x11 => match self.peek_key() {
                    Some(v) => {
                        self.cpu.regs[cpu::AX] = v as u32;
                        self.cpu.set_flag(cpu::ZF, false);
                    }
                    None => self.cpu.set_flag(cpu::ZF, true),
                },
                // シフト状態。**BDAに置いてある値をそのまま返す**
                0x02 | 0x12 => {
                    let f = self.read8(BDA_KB_FLAG1) as u32;
                    self.cpu.regs[cpu::AX] = (self.cpu.regs[cpu::AX] & 0xFF00) | f;
                }
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
                    self.devices
                        .cmos
                        .set_time_bcd((cx >> 8) as u8, cx as u8, (dx >> 8) as u8);
                }
                // AH=05: RTCの日付を設定する (DOSの `DATE`)
                0x05 => {
                    let cx = self.cpu.regs[cpu::CX];
                    let dx = self.cpu.regs[cpu::DX];
                    self.devices.cmos.set_date_bcd(
                        (cx >> 8) as u8,
                        cx as u8,
                        (dx >> 8) as u8,
                        dx as u8,
                    );
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
    /// 8042から取ったスキャンコードを、修飾キーの状態を見ながら
    /// **BIOSデータエリアの待ち行列へ積む**。IRQ1のたびに呼ばれる
    /// **1回の割り込みで1バイトだけ**処理する。
    ///
    /// 実機の8042はバイトごとにIRQ1を上げるので、ISRも1バイトずつ受ける。
    /// ここでまとめて吸い出すと、**16枠しかないBDAの待ち行列が溢れる**。
    /// 実際、貼り付けのように一度に流し込んだとき15文字で切れた
    /// (16枠 - 空判定用の1枠 = 15)。
    fn keyboard_isr(&mut self) {
        {
            if !self.devices.keyboard.has_data() {
                return;
            }
            let sc = self.devices.keyboard.read_data();
            if sc == 0xFF || sc == 0xE0 {
                return;
            }
            let released = sc & 0x80 != 0;
            let code = sc & 0x7F;
            // 修飾キーは文字にならない。状態だけを更新してBDAへ書く
            let bit = match code {
                0x2A => Some(0x02u8), // 左Shift
                0x36 => Some(0x01),   // 右Shift
                0x1D => Some(0x04),   // Ctrl
                0x38 => Some(0x08),   // Alt
                _ => None,
            };
            if let Some(b) = bit {
                let mut f = self.read8(BDA_KB_FLAG1);
                if released {
                    f &= !b
                } else {
                    f |= b
                }
                self.write8(BDA_KB_FLAG1, f);
                return;
            }
            if released {
                return;
            }
            let flags = self.read8(BDA_KB_FLAG1);
            let ascii = scancode_to_ascii(code, flags & 0x03 != 0, flags & 0x04 != 0).unwrap_or(0);
            self.kbd_enqueue((code as u16) << 8 | ascii as u16);
        }
    }

    /// 待ち行列へ1つ積む。**いっぱいなら捨てる** (実機も同じで、そのとき鳴る)
    fn kbd_enqueue(&mut self, entry: u16) {
        let tail = self.read16(BDA_KB_TAIL);
        let next = if tail + 2 >= KB_BUF_END {
            KB_BUF_START
        } else {
            tail + 2
        };
        if next == self.read16(BDA_KB_HEAD) {
            return; // 満杯
        }
        self.write16(BDA_SEG + tail as u32, entry);
        self.write16(BDA_KB_TAIL, next);
    }

    /// 待ち行列から1つ取り出す
    fn take_key(&mut self) -> Option<u16> {
        let head = self.read16(BDA_KB_HEAD);
        if head == self.read16(BDA_KB_TAIL) {
            return None;
        }
        let v = self.read16(BDA_SEG + head as u32);
        let next = if head + 2 >= KB_BUF_END {
            KB_BUF_START
        } else {
            head + 2
        };
        self.write16(BDA_KB_HEAD, next);
        Some(v)
    }

    /// 取らずに覗く。**待ち行列があるので預かる必要が無くなった**
    fn peek_key(&mut self) -> Option<u16> {
        let head = self.read16(BDA_KB_HEAD);
        if head == self.read16(BDA_KB_TAIL) {
            return None;
        }
        Some(self.read16(BDA_SEG + head as u32))
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
            // タブは**8桁ごとの停留所まで進む**。扱っていなかったので、
            // タブ文字そのもの (CP437では ○) を画面に書いていた
            b'\t' => col = (col / 8 + 1) * 8,
            _ => {
                let addr = bus::VRAM_TEXT_BASE + ((row * bus::TEXT_COLS + col) * 2) as u32;
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

    // 引数8個は INT 10h AH=06/07 のレジスタ割り当て (CH/CL/DH/DL/AL/BH/方向)
    // がそのまま並んだもの。束ねる構造体を作ると対応が読めなくなる
    #[allow(clippy::too_many_arguments)]
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
        let n = if lines == 0 {
            bottom - top + 1
        } else {
            lines as usize
        };

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
/// [`crate::dev::isa::kbd::scancode_shift`] のちょうど逆向きにあたる。
fn scancode_to_ascii(sc: u8, shift: bool, ctrl: bool) -> Option<u8> {
    const PLAIN: &[(u8, u8)] = &[
        (0x02, b'1'),
        (0x03, b'2'),
        (0x04, b'3'),
        (0x05, b'4'),
        (0x06, b'5'),
        (0x07, b'6'),
        (0x08, b'7'),
        (0x09, b'8'),
        (0x0A, b'9'),
        (0x0B, b'0'),
        (0x0C, b'-'),
        (0x0D, b'='),
        (0x1A, b'['),
        (0x1B, b']'),
        (0x27, b';'),
        (0x28, b'\''),
        (0x29, b'`'),
        (0x2B, b'\\'),
        (0x33, b','),
        (0x34, b'.'),
        (0x35, b'/'),
    ];
    const SHIFTED: &[(u8, u8)] = &[
        (0x02, b'!'),
        (0x03, b'@'),
        (0x04, b'#'),
        (0x05, b'$'),
        (0x06, b'%'),
        (0x07, b'^'),
        (0x08, b'&'),
        (0x09, b'*'),
        (0x0A, b'('),
        (0x0B, b')'),
        (0x0C, b'_'),
        (0x0D, b'+'),
        (0x1A, b'{'),
        (0x1B, b'}'),
        (0x27, b':'),
        (0x28, b'"'),
        (0x29, b'~'),
        (0x2B, b'|'),
        (0x33, b'<'),
        (0x34, b'>'),
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
    // **Ctrl を押しながらだと制御文字になる。**
    //
    // Ctrl+A〜Z が 0x01〜0x1A なのは、ASCIIの英大文字が 0x41〜0x5A に並んでいて、
    // **上位3ビットを落とすと 1〜26 になる**という配置のおかげである。
    // 端末が Ctrl+C で止まり Ctrl+D で終わるのは、この引き算の名残でしかない。
    //
    // ここを通していなかったので、Ctrl は8042まで届いていたのに
    // 文字にする段で捨てられ、Ctrl+C がただの `c` になっていた。
    if ctrl {
        // 英字は素の文字から機械的に作れる
        if let Some(c) = [ROW_Q, ROW_A, ROW_Z]
            .iter()
            .zip([0x10u8, 0x1E, 0x2C])
            .find_map(|(row, base)| letter(base, row))
        {
            return Some(c.to_ascii_uppercase() & 0x1F);
        }
        // 記号はASCIIの並びから外れるので個別に持つ
        return Some(match sc {
            0x1A => 0x1B, // Ctrl+[ は Esc と同じ
            0x2B => 0x1C, // Ctrl+\\
            0x1B => 0x1D, // Ctrl+]
            0x07 => 0x1E, // Ctrl+6
            0x0C => 0x1F, // Ctrl+-
            0x39 => b' ',
            0x1C => b'\r',
            0x0E => 8,
            0x0F => b'\t',
            0x01 => 27,
            _ => 0, // 対応する制御文字が無いキーは文字を持たない
        });
    }

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
