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
//! なおA20を実際に開閉する意味が出るのは Tier 4 (1MBを超えるメモリ) からで、
//! 今は**手順が完了すること**だけが要る。

/// ステータス bit0: 出力バッファにデータあり (CPUが読める)
pub const STATUS_OBF: u8 = 1 << 0;
/// ステータス bit1: 入力バッファが埋まっている (CPUは書けない)
pub const STATUS_IBF: u8 = 1 << 1;

/// 出力ポート bit1 が A20 の開閉
pub const OUTPORT_A20: u8 = 1 << 1;

#[derive(Debug)]
pub struct Kbd8042 {
    /// CPUが 0x60 から読む1バイト
    output_buf: u8,
    /// 出力バッファにデータがあるか
    has_output: bool,
    /// 0x64 に書かれたコマンドのうち、続けてデータを待つもの
    pending: Option<u8>,
    /// 出力ポート。bit0=リセット bit1=A20
    pub output_port: u8,
    /// キー入力の待ち行列
    keys: std::collections::VecDeque<u8>,
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
            pending: None,
            // 起動時からA20は開いている扱いにする。実機のBIOSも大抵そうする
            output_port: OUTPORT_A20 | 1,
            keys: std::collections::VecDeque::new(),
            irq_asserted: false,
        }
    }

    /// A20が開いているか (Tier 4 で 1MB 超のアドレスを扱うときに効いてくる)
    pub fn a20_enabled(&self) -> bool {
        self.output_port & OUTPORT_A20 != 0
    }

    /// ステータスポート (0x64) の読み出し。
    /// **入力バッファは常に空**にしてある — ホスト側は待たせる理由が無い
    pub fn read_status(&self) -> u8 {
        let mut s = 0;
        if self.has_output || !self.keys.is_empty() {
            s |= STATUS_OBF;
        }
        s // IBFは立てない (常に書き込みを受け付ける)
    }

    /// データポート (0x60) の読み出し
    pub fn read_data(&mut self) -> u8 {
        // 読まれたら割り込みの主張を下ろす。次のバイトがあれば改めて上げ直す
        self.irq_asserted = false;
        if self.has_output {
            self.has_output = false;
            return self.output_buf;
        }
        // 空を読まれたら 0xFF を返す。0 は正当なスキャンコードと紛らわしい
        self.keys.pop_front().unwrap_or(0xFF)
    }

    /// 今このタイミングでIRQ1を上げるべきか。**1バイトにつき1回だけ真を返す**
    pub fn take_irq(&mut self) -> bool {
        if self.has_data() && !self.irq_asserted {
            self.irq_asserted = true;
            true
        } else {
            false
        }
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
            // キーボードの無効化/有効化。A20操作の前後で挟むのが定石
            0xAD | 0xAE => {}
            // セルフテスト。0x55 が「正常」の合図
            0xAA => {
                self.output_buf = 0x55;
                self.has_output = true;
            }
            // インタフェーステスト
            0xAB => {
                self.output_buf = 0x00;
                self.has_output = true;
            }
            // コマンドバイトの読み書き
            0x20 => {
                self.output_buf = 0x45;
                self.has_output = true;
            }
            0x60 => self.pending = Some(0x60),
            // リセットパルス (再起動)。ここでは何もしない
            0xFE => {}
            _ => {}
        }
    }

    /// データポート (0x60) への書き込み
    pub fn write_data(&mut self, val: u8) {
        match self.pending.take() {
            Some(0xD1) => self.output_port = val,
            Some(0x60) => {} // コマンドバイトの設定
            _ => {
                // キーボード自身へのコマンド。ACK (0xFA) を返しておく
                self.output_buf = 0xFA;
                self.has_output = true;
            }
        }
    }

    /// ホスト側からスキャンコードを流し込む
    pub fn feed(&mut self, codes: &[u8]) {
        self.keys.extend(codes.iter().copied());
    }

    /// 読ませるデータが残っているか。**IRQ1を上げ続ける条件**でもある。
    /// キーボードは割り込み駆動で、OSはハンドラの中で 0x60 を1バイト読む
    pub fn has_data(&self) -> bool {
        self.has_output || !self.keys.is_empty()
    }

    /// ASCII文字を押して離す。
    ///
    /// PCのキーボードは文字ではなく**キーの位置**を送る。押すと「メイク」、
    /// 離すと「ブレーク」(メイク + 0x80) が流れ、文字への変換はOSの仕事である。
    /// 配列を変えられるのも、Shiftが別のキーとして届くのも、この設計のおかげ。
    pub fn type_ascii(&mut self, s: &str) {
        const LSHIFT: u8 = 0x2A;
        for ch in s.chars() {
            let Some(code) = scancode(ch) else { continue };
            // 大文字は「Shiftを押しながら同じキーを叩く」として送る。
            // 文字コードを渡す装置ではないので、実機と同じ手順を踏む
            let shifted = ch.is_ascii_uppercase();
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

/// ASCII文字 → スキャンコード セット1。
///
/// 並びがアルファベット順でないのは、**キーボード上の物理的な位置に振られた
/// 番号**だからである。左上から数えると Q=0x10、W=0x11 と続く —
/// QWERTY配列そのものが番号の並びになっている。
pub fn scancode(ch: char) -> Option<u8> {
    const ROW_Q: &str = "qwertyuiop";
    const ROW_A: &str = "asdfghjkl";
    const ROW_Z: &str = "zxcvbnm";
    let c = ch.to_ascii_lowercase();
    if let Some(i) = ROW_Q.find(c) {
        return Some(0x10 + i as u8);
    }
    if let Some(i) = ROW_A.find(c) {
        return Some(0x1E + i as u8);
    }
    if let Some(i) = ROW_Z.find(c) {
        return Some(0x2C + i as u8);
    }
    Some(match c {
        '1'..='9' => 0x02 + (c as u8 - b'1'),
        '0' => 0x0B,
        '\n' | '\r' => 0x1C,
        ' ' => 0x39,
        '\x08' => 0x0E,
        '-' => 0x0C,
        '=' => 0x0D,
        '.' => 0x34,
        ',' => 0x33,
        '/' => 0x35,
        ';' => 0x27,
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
