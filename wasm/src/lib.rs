//! ブラウザ向けのWASMラッパー。
//!
//! 最終的にブラウザでOSを動かすのがこのプロジェクトのゴールなので、
//! **ネイティブとブラウザで実行速度がどれだけ違うか**は早い段階で
//! 知っておきたい。ここは今のところベンチのためだけの薄い層で、
//! Tier 2d で画面と入力を足して本格的なフロントになる。
//!
//! 時間の計測はJS側の `performance.now()` で行う。
//! `std::time::Instant` は wasm32-unknown-unknown では動かない
//! (プラットフォームの時計に触れないため) 一方、`performance.now()` は
//! ブラウザが提供する単調増加の高分解能タイマーなので、そちらを使う。

use rustx86_core::Machine;
use wasm_bindgen::prelude::*;

/// ベンチ用ワークロード (`asm/bench.asm` の成果物) をwasmバイナリに埋め込む。
///
/// ページ側からfetchさせてもよいが、埋め込んでおくと `file://` で開いても
/// 動き、測定対象がバイナリと必ず一致する
const BENCH_SECTOR: &[u8] = include_bytes!("../../asm/bench.bin");

#[wasm_bindgen]
pub struct Emulator {
    m: Machine,
}

#[wasm_bindgen]
impl Emulator {
    /// ブートセクタ (512バイト、末尾 0x55AA) を読み込んで CS:IP=0000:7C00 から開始
    #[wasm_bindgen(constructor)]
    pub fn new(sector: &[u8]) -> Result<Emulator, JsError> {
        let mut m = Machine::new();
        m.load_boot_sector(sector).map_err(|e| JsError::new(&e))?;
        Ok(Emulator { m })
    }

    /// 埋め込みのベンチ用ワークロードで初期化する
    pub fn bench() -> Emulator {
        let mut m = Machine::new();
        m.load_boot_sector(BENCH_SECTOR)
            .expect("埋め込みワークロードが壊れている");
        Emulator { m }
    }

    /// HLTするか上限まで実行し、実行した命令数を返す。
    ///
    /// **この呼び出しの間ブラウザのメインスレッドは止まる**。計測としては
    /// その方が正しい (途中でイベントループに戻ると他の処理が混ざる) ので、
    /// 分割せず一息に走らせている。呼ぶ側が画面表示を先に更新しておくこと
    pub fn run(&mut self, max_instructions: f64) -> f64 {
        self.m.run(max_instructions as u64) as f64
    }

    pub fn halted(&self) -> bool {
        self.m.halted
    }

    /// INT 10h テレタイプ出力の蓄積 (Tier 2 で本物のコンソールに置き換わる)
    pub fn console(&self) -> String {
        self.m.console_string()
    }
}

/// 埋め込みワークロードの命令数は固定なので、ネイティブ側の測定と直接比較できる
#[wasm_bindgen]
pub fn bench_sector_len() -> usize {
    BENCH_SECTOR.len()
}

// ---------- OSを動かすための口 ----------

#[wasm_bindgen]
impl Emulator {
    /// ディスクイメージ (フロッピー) から起動する
    pub fn from_disk(image: &[u8]) -> Result<Emulator, JsError> {
        let mut m = Machine::new();
        m.boot_from_disk(image.to_vec())
            .map_err(|e| JsError::new(&e))?;
        Ok(Emulator { m })
    }

    /// 指定した命令数だけ進める。**1フレーム分の仕事**として呼ぶ。
    ///
    /// HLTで止まっていても抜けない — タイマ割り込みで起きるのを待つ必要があるため。
    /// アイドル中のOSは「HLTして割り込みを待つ」を繰り返している
    pub fn run_slice(&mut self, instructions: f64) {
        let n = instructions as u64;
        for _ in 0..n {
            self.m.step();
        }
    }

    /// テキストVRAM (80×25、文字と属性が交互) の先頭ポインタ。
    /// JS側はwasmのメモリを直接読む — コピーを作らないため
    pub fn text_vram_ptr(&self) -> *const u8 {
        self.m.text_vram().as_ptr()
    }

    pub fn text_vram_len(&self) -> usize {
        rustx86_core::bus::TEXT_LEN
    }

    pub fn text_cols(&self) -> usize {
        rustx86_core::bus::TEXT_COLS
    }

    pub fn text_rows(&self) -> usize {
        rustx86_core::bus::TEXT_ROWS
    }

    /// 画面が書き換わったか。**読むと下りる**ので描画の要否判定に使う。
    /// 毎フレーム画面を組み立て直すのは無駄が大きい
    pub fn take_vram_dirty(&mut self) -> bool {
        self.m.take_vram_dirty()
    }

    /// 文字列をキーボードから打つ。8042にスキャンコードが流れ、IRQ1が上がる
    pub fn type_text(&mut self, s: &str) {
        self.m.devices.keyboard.type_ascii(s);
    }

    /// 生のスキャンコードを流す
    pub fn send_scancode(&mut self, code: u8) {
        self.m.devices.keyboard.feed(&[code]);
    }

    /// キーの上げ下げを送る。`code` は `KeyboardEvent.code` (物理キーの識別子)。
    ///
    /// 文字ではなく**キーの位置**を渡すのが要点である。こうすると Ctrl も Esc も
    /// 矢印も特別扱いが要らず、修飾キーの組み立てはゲストのOSがやる。
    /// 返り値は「そのキーを知っているか」
    pub fn key(&mut self, code: &str, down: bool) -> bool {
        self.m.devices.keyboard.key(code, down)
    }

    /// カーソルの行 (CRTCが持っている)
    pub fn cursor_row(&self) -> usize {
        self.m.cursor_pos().0
    }

    /// カーソルの桁
    pub fn cursor_col(&self) -> usize {
        self.m.cursor_pos().1
    }
}
