//! 8042 キーボードコントローラ。
//!
//! 名前はキーボード用だが、**A20ゲートの開閉という無関係な仕事を背負わされている**。
//!
//! ## なぜキーボードチップがメモリ線を握っているのか
//!
//! 8086は20本のアドレス線しか持たず、`0xFFFF:0xFFFF` は1MBを超えた瞬間に
//! 0番地へ折り返した。一部のソフトがこの折り返しを**前提に**書かれていたため、
//! 286で21本目 (A20) が増えたとき、そのままでは互換性が壊れた。
//!
//! IBMの解決策は「A20を切れるようにする」だった。だが専用チップを足す予算も
//! 空きポートも無く、**たまたま出力ピンが余っていた**キーボードコントローラに
//! 繋いだ。以来「メモリを1MBより先まで使うには、まずキーボードに話しかける」
//! という奇妙な手順がPCの作法になった。
//!
//! ELKSもLinuxも起動時にこれを踏む。ここが応答しないとOSは永久に待つ。
//!
//! なおA20を実際に開閉する意味が出るのは Tier 3 (1MBを超えるメモリ) からで、
//! 今は**手順が完了すること**だけが要る。

/// ステータス bit0: 出力バッファにデータあり (CPUが読める)
pub const STATUS_OBF: u8 = 1 << 0;
/// ステータス bit1: 入力バッファが埋まっている (CPUは書けない)
pub const STATUS_IBF: u8 = 1 << 1;

/// 出力ポート bit1 が A20 の開閉
pub const OUTPORT_A20: u8 = 1 << 1;

/// ステータス bit4: キーロック (1 = ロックされていない)。
/// 0 のままだと Linux が "Keylock active" と警告する
pub const STATUS_UNLOCKED: u8 = 1 << 4;
/// ステータス bit5: 出力バッファの中身が**第2ポート (AUX = マウス)** から来た
pub const STATUS_AUX: u8 = 1 << 5;

/// コマンドバイト (0x20 で読み 0x60 で書く) のビット
pub const CMD_KBD_IRQ: u8 = 1 << 0;
pub const CMD_AUX_IRQ: u8 = 1 << 1;
pub const CMD_SYSFLAG: u8 = 1 << 2;
pub const CMD_KBD_DISABLE: u8 = 1 << 4;
pub const CMD_AUX_DISABLE: u8 = 1 << 5;
pub const CMD_TRANSLATE: u8 = 1 << 6;

#[derive(Debug)]
pub struct Kbd8042 {
    /// 8042 自身の返事 (セルフテストの 0x55 など)。キーより先に読ませる
    output_buf: u8,
    /// 出力バッファにデータがあるか
    has_output: bool,
    /// 出力バッファの中身が AUX (マウス) 側か
    output_is_aux: bool,
    /// 0x64 に書かれたコマンドのうち、続けてデータを待つもの
    pending: Option<u8>,
    /// 出力ポート。bit0=リセット bit1=A20
    pub output_port: u8,
    /// コマンドバイト。**実体を持つ** — Linux は AUX を止めて (0xA7) 読み返し、
    /// bit5 が立っていなければ "Failed to disable AUX port" と疑う。固定値を
    /// 返していた頃はそこで第2ポート無しと判定されていた
    pub command_byte: u8,
    /// キー入力の待ち行列
    keys: std::collections::VecDeque<u8>,
    /// 第2ポートの向こうのマウス
    pub mouse: super::Mouse,
    /// 今あるデータについて、既に割り込みを上げたか。
    ///
    /// **これが無いと文字が化ける。** 割り込みを「データがある間ずっと」上げると、
    /// CPUが読み終えた後にも余分な割り込みが1発残り、OSは空のバッファを読んで
    /// でたらめなスキャンコードを受け取る。実機の割り込み線は**変化したときに1回**
    /// 動くもので、状態を垂れ流すものではない
    irq_asserted: bool,
}

impl Default for Kbd8042 {
    fn default() -> Self {
        Self::new()
    }
}

impl Kbd8042 {
    pub fn new() -> Self {
        Self {
            output_buf: 0,
            has_output: false,
            output_is_aux: false,
            pending: None,
            // 起動時からA20は開いている扱いにする。実機のBIOSも大抵そうする
            output_port: OUTPORT_A20 | 1,
            // BIOSが渡す既定: 変換あり・システムフラグ・キーボード割り込み許可。
            // AUX の割り込みは OS が開ける (Linux の i8042 はそうする)
            command_byte: CMD_TRANSLATE | CMD_SYSFLAG | CMD_KBD_IRQ,
            keys: std::collections::VecDeque::new(),
            mouse: super::Mouse::new(),
            irq_asserted: false,
        }
    }

    /// A20が開いているか (Tier 3 で 1MB 超のアドレスを扱うときに効いてくる)
    pub fn a20_enabled(&self) -> bool {
        self.output_port & OUTPORT_A20 != 0
    }

    /// ステータスポート (0x64) の読み出し。
    /// **入力バッファは常に空**にしてある — ホスト側は待たせる理由が無い
    pub fn read_status(&self) -> u8 {
        let mut s = STATUS_UNLOCKED;
        if self.has_output {
            s |= STATUS_OBF;
            if self.output_is_aux {
                s |= STATUS_AUX;
            }
        } else if !self.keys.is_empty() {
            s |= STATUS_OBF;
        } else if self.aux_ready() {
            s |= STATUS_OBF | STATUS_AUX;
        }
        s // IBFは立てない (常に書き込みを受け付ける)
    }

    /// マウスのバイトが出力バッファへ上がれる状態か (AUXが止められていない)
    fn aux_ready(&self) -> bool {
        self.command_byte & CMD_AUX_DISABLE == 0 && self.mouse.has_output()
    }

    /// データポート (0x60) の読み出し
    pub fn read_data(&mut self) -> u8 {
        // 読まれたら割り込みの主張を下ろす。次のバイトがあれば改めて上げ直す
        self.irq_asserted = false;
        if self.has_output {
            self.has_output = false;
            self.output_is_aux = false;
            return self.output_buf;
        }
        if let Some(k) = self.keys.pop_front() {
            return k;
        }
        if self.aux_ready() {
            return self.mouse.pop().unwrap_or(0xFF);
        }
        // 空を読まれたら 0xFF を返す。0 は正当なスキャンコードと紛らわしい
        0xFF
    }

    /// まだゲストへ配っていないスキャンコードの数。
    /// **配送は1タイマー刻みに1バイト**なので、ここが空くのを待たずに
    /// 流し込むと、貼り付けた文字が行列に積み上がるだけになる
    pub fn backlog(&self) -> usize {
        self.keys.len()
    }

    /// 今このタイミングで上げるべき割り込み線 (1 = キーボード / 12 = マウス)。
    /// **1バイトにつき1回だけ Some を返す**。どちらの線かは次に読まれるバイトの
    /// 出どころで決まり、コマンドバイトでその線の割り込みが切られていれば上げない
    pub fn take_irq(&mut self) -> Option<u8> {
        if self.irq_asserted {
            return None;
        }
        let aux = if self.has_output {
            self.output_is_aux
        } else if !self.keys.is_empty() {
            false
        } else if self.aux_ready() {
            true
        } else {
            return None;
        };
        let enabled = self.command_byte & if aux { CMD_AUX_IRQ } else { CMD_KBD_IRQ } != 0;
        if !enabled {
            return None;
        }
        self.irq_asserted = true;
        Some(if aux { 12 } else { 1 })
    }

    /// コマンドポート (0x64) への書き込み
    pub fn write_command(&mut self, cmd: u8) {
        match cmd {
            // 出力ポートを読ませる。A20の状態を確認するときに使う
            0xD0 => {
                self.output_buf = self.output_port;
                self.has_output = true;
            }
            // 次に 0x60 へ書かれる値を出力ポートにする = **A20の開閉**
            0xD1 => self.pending = Some(0xD1),
            // キーボードの無効化/有効化。A20操作の前後で挟むのが定石。
            // コマンドバイトに写すだけで、配送は止めない (止めると、再有効化を
            // 忘れたゲストのキーが永久に届かなくなる — 今まで動いていた物を壊さない)
            0xAD => self.command_byte |= CMD_KBD_DISABLE,
            0xAE => self.command_byte &= !CMD_KBD_DISABLE,
            // 第2ポート (マウス) の無効化/有効化。**こちらは配送も止める** —
            // Linux は止めた状態で 0x20 を読み返して bit5 を確かめる
            0xA7 => self.command_byte |= CMD_AUX_DISABLE,
            0xA8 => self.command_byte &= !CMD_AUX_DISABLE,
            // 第2ポートのインタフェーステスト。0x00 = 正常
            0xA9 => self.reply(0x00),
            // セルフテスト。0x55 が「正常」の合図
            0xAA => self.reply(0x55),
            // インタフェーステスト
            0xAB => self.reply(0x00),
            // コマンドバイトの読み書き
            0x20 => self.reply(self.command_byte),
            0x60 => self.pending = Some(0x60),
            // 次のバイトを出力バッファへ: 0xD2 はキーボード側として、0xD3 は
            // AUX 側として見せる (ループバック)。Linux の i8042 は 0xD3 で
            // 「AUX の経路が生きているか」を確かめる — 書いた値が AUX の印つきで
            // 返ってこなければ第2ポート無しと判断する
            0xD2 | 0xD3 => self.pending = Some(cmd),
            // 次のバイトをマウスへ送る
            0xD4 => self.pending = Some(0xD4),
            // リセットパルス (再起動)。ここでは何もしない
            0xFE => {}
            _ => {}
        }
    }

    /// 8042 自身の返事を出力バッファへ (キーボード側として)
    fn reply(&mut self, val: u8) {
        self.output_buf = val;
        self.has_output = true;
        self.output_is_aux = false;
    }

    /// データポート (0x60) への書き込み
    pub fn write_data(&mut self, val: u8) {
        match self.pending.take() {
            Some(0xD1) => self.output_port = val,
            Some(0x60) => self.command_byte = val,
            Some(0xD2) => self.reply(val),
            Some(0xD3) => {
                self.output_buf = val;
                self.has_output = true;
                self.output_is_aux = true;
            }
            Some(0xD4) => self.mouse.command(val),
            _ => {
                // キーボード自身へのコマンド。ACK (0xFA) を返しておく
                self.reply(0xFA);
            }
        }
    }

    /// ホスト側のマウスの動き (dx: 右が正, dy: 下が正, buttons: bit0=左 bit1=右 bit2=中)
    pub fn mouse_motion(&mut self, dx: i32, dy: i32, buttons: u8) {
        self.mouse.motion(dx, dy, buttons);
    }

    /// ホスト側からスキャンコードを流し込む
    pub fn feed(&mut self, codes: &[u8]) {
        self.keys.extend(codes.iter().copied());
    }

    /// 読ませるデータが残っているか。**IRQ1を上げ続ける条件**でもある。
    /// キーボードは割り込み駆動で、OSはハンドラの中で 0x60 を1バイト読む
    pub fn has_data(&self) -> bool {
        self.has_output || !self.keys.is_empty() || self.aux_ready()
    }

    /// ASCII文字を押して離す。
    ///
    /// PCのキーボードは文字ではなく**キーの位置**を送る。押すと「メイク」、
    /// 離すと「ブレーク」(メイク + 0x80) が流れ、文字への変換はOSの仕事である。
    /// 配列を変えられるのも、Shiftが別のキーとして届くのも、この設計のおかげ。
    pub fn type_ascii(&mut self, s: &str) {
        const LSHIFT: u8 = 0x2A;
        for ch in s.chars() {
            let Some((code, shifted)) = scancode_shift(ch) else {
                continue;
            };
            // Shiftは独立したキーとして届く。文字コードを渡す装置ではないので、
            // 実機と同じ手順を踏む
            if shifted {
                self.keys.push_back(LSHIFT);
            }
            self.keys.push_back(code);
            self.keys.push_back(code | 0x80);
            if shifted {
                self.keys.push_back(LSHIFT | 0x80);
            }
        }
    }
}

/// ASCII文字 → (スキャンコード, Shiftが要るか)。**US配列**での対応。
///
/// ゲスト (ELKS) はUS配列の対応表しか持たないので、こちらもUS配列で組み立てる。
/// JIS配列の実機から使うときは、ブラウザが解釈した文字をここへ渡せば
/// 見たままの文字が入る — 位置ではなく文字で辻褄を合わせるということである。
///
/// 数字段の記号 (`!` から `)`) が数字のShiftになっているのはタイプライタからの
/// 引き継ぎで、キーの位置に意味があるわけではない。
pub fn scancode_shift(ch: char) -> Option<(u8, bool)> {
    // 上段・中段・下段。左上から数えた位置がそのまま番号になっている
    const ROW_Q: &str = "qwertyuiop";
    const ROW_A: &str = "asdfghjkl";
    const ROW_Z: &str = "zxcvbnm";
    // (スキャンコード, Shift無しの文字, Shift有りの文字)
    const SYMBOLS: [(u8, char, char); 21] = [
        (0x02, '1', '!'),
        (0x03, '2', '@'),
        (0x04, '3', '#'),
        (0x05, '4', '$'),
        (0x06, '5', '%'),
        (0x07, '6', '^'),
        (0x08, '7', '&'),
        (0x09, '8', '*'),
        (0x0A, '9', '('),
        (0x0B, '0', ')'),
        (0x0C, '-', '_'),
        (0x0D, '=', '+'),
        (0x1A, '[', '{'),
        (0x1B, ']', '}'),
        (0x27, ';', ':'),
        (0x28, '\'', '"'),
        (0x29, '`', '~'),
        (0x2B, '\\', '|'),
        (0x33, ',', '<'),
        (0x34, '.', '>'),
        (0x35, '/', '?'),
    ];

    if ch.is_ascii_alphabetic() {
        let lower = ch.to_ascii_lowercase();
        let shifted = ch.is_ascii_uppercase();
        if let Some(i) = ROW_Q.find(lower) {
            return Some((0x10 + i as u8, shifted));
        }
        if let Some(i) = ROW_A.find(lower) {
            return Some((0x1E + i as u8, shifted));
        }
        if let Some(i) = ROW_Z.find(lower) {
            return Some((0x2C + i as u8, shifted));
        }
    }
    for (code, plain, shift) in SYMBOLS {
        if ch == plain {
            return Some((code, false));
        }
        if ch == shift {
            return Some((code, true));
        }
    }
    Some(match ch {
        '\n' | '\r' => (0x1C, false),
        ' ' => (0x39, false),
        '\t' => (0x0F, false),
        '\x08' | '\x7f' => (0x0E, false),
        '\x1b' => (0x01, false),
        _ => return None,
    })
}

/// ブラウザの `KeyboardEvent.code` からスキャンコード セット1 への対応表。
///
/// `code` は「押された**物理的なキーの位置**」を表す識別子で、
/// スキャンコードとまったく同じ考え方をしている。だから変換は素直な対応付けで済み、
/// **Ctrl も Shift も矢印も特別扱いが要らない** — OSが自分で組み立てる。
///
/// 文字に直してから渡す方式だと、Ctrl+C も Esc も表現できない。
/// キーボードは文字を送る装置ではなく、キーの上げ下げを送る装置である。
pub fn scancode_for_code(code: &str) -> Option<u8> {
    // 文字キーは配列順に並んでいる (左上から数えた位置がそのまま番号)
    const ROW_Q: [&str; 10] = [
        "KeyQ", "KeyW", "KeyE", "KeyR", "KeyT", "KeyY", "KeyU", "KeyI", "KeyO", "KeyP",
    ];
    const ROW_A: [&str; 9] = [
        "KeyA", "KeyS", "KeyD", "KeyF", "KeyG", "KeyH", "KeyJ", "KeyK", "KeyL",
    ];
    const ROW_Z: [&str; 7] = ["KeyZ", "KeyX", "KeyC", "KeyV", "KeyB", "KeyN", "KeyM"];

    if let Some(i) = ROW_Q.iter().position(|k| *k == code) {
        return Some(0x10 + i as u8);
    }
    if let Some(i) = ROW_A.iter().position(|k| *k == code) {
        return Some(0x1E + i as u8);
    }
    if let Some(i) = ROW_Z.iter().position(|k| *k == code) {
        return Some(0x2C + i as u8);
    }
    if let Some(d) = code.strip_prefix("Digit") {
        let c = d.as_bytes()[0];
        return Some(if c == b'0' { 0x0B } else { 0x02 + (c - b'1') });
    }
    if let Some(f) = code.strip_prefix("F") {
        if let Ok(n) = f.parse::<u8>() {
            if (1..=10).contains(&n) {
                return Some(0x3A + n); // F1 = 0x3B
            }
        }
    }
    Some(match code {
        "Escape" => 0x01,
        "Minus" => 0x0C,
        "Equal" => 0x0D,
        "Backspace" => 0x0E,
        "Tab" => 0x0F,
        "BracketLeft" => 0x1A,
        "BracketRight" => 0x1B,
        "Enter" | "NumpadEnter" => 0x1C,
        "ControlLeft" | "ControlRight" => 0x1D,
        "Semicolon" => 0x27,
        "Quote" => 0x28,
        "Backquote" => 0x29,
        "ShiftLeft" => 0x2A,
        "Backslash" => 0x2B,
        "Comma" => 0x33,
        "Period" => 0x34,
        "Slash" => 0x35,
        "ShiftRight" => 0x36,
        "AltLeft" | "AltRight" => 0x38,
        "Space" => 0x39,
        "CapsLock" => 0x3A,
        // 矢印などは 0xE0 を前置する拡張キー。ここでは基本形だけ返し、
        // 前置は [`Kbd8042::key`] が付ける
        "ArrowUp" => 0x48,
        "ArrowLeft" => 0x4B,
        "ArrowRight" => 0x4D,
        "ArrowDown" => 0x50,
        "Home" => 0x47,
        "End" => 0x4F,
        "PageUp" => 0x49,
        "PageDown" => 0x51,
        "Insert" => 0x52,
        "Delete" => 0x53,
        _ => return None,
    })
}

/// 0xE0 を前置して送るキー (PC/ATで後から足されたキー群)。
///
/// 8086時代のキーボードには無かったので番号が空いておらず、
/// **「次のバイトは拡張キー」という目印を先に送る**方式で追加された。
/// テンキーの Enter と本体の Enter が区別できるのもこの仕組みによる。
fn is_extended(code: &str) -> bool {
    matches!(
        code,
        "ArrowUp"
            | "ArrowDown"
            | "ArrowLeft"
            | "ArrowRight"
            | "Home"
            | "End"
            | "PageUp"
            | "PageDown"
            | "Insert"
            | "Delete"
            | "ControlRight"
            | "AltRight"
            | "NumpadEnter"
    )
}

impl Kbd8042 {
    /// キーの上げ下げを送る。`down=false` ならブレークコード (最上位ビットを立てる)
    pub fn key(&mut self, code: &str, down: bool) -> bool {
        let Some(sc) = scancode_for_code(code) else {
            return false;
        };
        if is_extended(code) {
            self.keys.push_back(0xE0);
        }
        self.keys.push_back(if down { sc } else { sc | 0x80 });
        true
    }
}

impl Kbd8042 {
    pub fn save(&self, w: &mut crate::snapshot::Writer) {
        w.u8(self.output_buf);
        w.bool(self.has_output);
        w.bool(self.output_is_aux);
        w.opt_u8(self.pending);
        w.u8(self.output_port);
        w.u8(self.command_byte);
        w.bool(self.irq_asserted);
        let keys: Vec<u8> = self.keys.iter().copied().collect();
        w.bytes(&keys);
        self.mouse.save(w);
    }

    pub fn load(&mut self, r: &mut crate::snapshot::Reader) -> Result<(), String> {
        self.output_buf = r.u8()?;
        self.has_output = r.bool()?;
        self.output_is_aux = r.bool()?;
        self.pending = r.opt_u8()?;
        self.output_port = r.u8()?;
        self.command_byte = r.u8()?;
        self.irq_asserted = r.bool()?;
        self.keys = r.bytes()?.into();
        self.mouse.load(r)?;
        Ok(())
    }
}
