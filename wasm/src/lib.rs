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
