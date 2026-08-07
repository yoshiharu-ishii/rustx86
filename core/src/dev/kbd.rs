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
        if self.has_output {
            self.has_output = false;
            return self.output_buf;
        }
        self.keys.pop_front().unwrap_or(0)
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

    /// ホスト側からキーコードを流し込む
    pub fn feed(&mut self, codes: &[u8]) {
        self.keys.extend(codes.iter().copied());
    }
}
