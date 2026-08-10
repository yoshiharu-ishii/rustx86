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

#[wasm_bindgen]
extern "C" {
    /// パニックの内容をJS側へ渡す口。ページが `globalThis.__rustx86_panic` を
    /// 定義しておく (無ければ `console.error` にだけ出る)
    #[wasm_bindgen(js_name = __rustx86_panic)]
    fn report_panic(msg: &str);

    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(msg: &str);
}

/// パニックの内容をブラウザまで届ける。
///
/// **これが無いとJS側には `RuntimeError: unreachable` としか見えない。**
/// このエミュレータは「未実装のサービスや命令は黙って0を返さず即panicして
/// 正体を報告する」方針で作ってあり、`INT 10h AH=0x13 未実装` や
/// `unimplemented opcode 0x66 at 22c8:0759` という**名前**こそが価値である。
/// 素のwasmはその文字列を捨ててしまうので、ここで拾い直す。
///
/// パニックの後のwasmインスタンスは触れないので、**フックの中で渡しきる**。
/// 「後から取りに行く」形にはできない。
#[wasm_bindgen]
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        console_error(&msg);
        report_panic(&msg);
    }));
}

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

    /// 直近の呼び出し以降に、アイドル (HLT) の早送りが飛ばした仮想命令数。
    /// 読むとゼロに戻る。ランナーはこれを実時間に換算して待ち、
    /// **暇なゲストの時計が実時間より速く回らないように**する
    pub fn take_idle_skipped(&mut self) -> f64 {
        let v = self.m.idle_skipped;
        self.m.idle_skipped = 0;
        v as f64
    }

    pub fn halted(&self) -> bool {
        self.m.halted
    }

    /// INT 10h テレタイプ出力の蓄積 (Tier 2 で本物のコンソールに置き換わる)
    pub fn console(&self) -> String {
        self.m.console_string()
    }
}

/// コードページ437 の256文字をUnicodeへ写した表 (1本の文字列)。
///
/// **CLIの確認表示と同じ表をブラウザにも渡す**ための口である。
/// 別々に持つと「CLIでは出るのにブラウザでは化ける」が起きる。
/// DOSの画面は罫線とブロック文字 (0xB0-0xDF) で描かれているので、
/// ここを素通しにすると枠もロゴも壊れる。
#[wasm_bindgen]
pub fn cp437_table() -> String {
    rustx86_core::cp437::table_string()
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

    /// カーネルイメージ (+ initramfs) から 32bit Linux を起動する。
    /// bzImage / vmlinux (ELF) は中身で自動判別 — vmlinux なら自己解凍ステブが
    /// 無いぶん起動が4割速い。`ram_mb` はRAMサイズ (MB)。
    /// コンソールはシリアル (ttyS0) — `serial_out` / `serial_in` で読み書きする
    pub fn from_bzimage(
        kernel: &[u8],
        initrd: Option<Vec<u8>>,
        cmdline: &str,
        ram_mb: usize,
    ) -> Result<Emulator, JsError> {
        let mut m = Machine::with_profile(rustx86_core::MachineProfile::pc_32bit(ram_mb));
        m.boot_linux_with_initrd(kernel, cmdline, initrd.as_deref())
            .map_err(|e| JsError::new(&e))?;
        Ok(Emulator { m })
    }

    /// 起動済みスナップショットから機械を丸ごと復元する。
    ///
    /// 「シンプルなカーネルの起動に1分」への即効薬 — 一度起動した機械を
    /// 控えておき、次からはそこから始める (Firecrackerのsnapshot相当)。
    /// RAMサイズはスナップショットが暗黙に持つので引数は要らない
    pub fn from_snapshot(data: &[u8]) -> Result<Emulator, JsError> {
        let mut m = Machine::new();
        m.load_state(data).map_err(|e| JsError::new(&e))?;
        Ok(Emulator { m })
    }

    /// シリアル (UART) に溜まった出力を取り出して返す。**読むと消える** —
    /// 端末は差分だけ受け取ればよい。Linuxのコンソールはここに出る
    pub fn serial_out(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.m.devices.uart.tx)
    }

    /// シリアルへ入力を1バイト以上流す (キーボード → ttyS0)
    pub fn serial_in(&mut self, bytes: &[u8]) {
        self.m.devices.uart.feed(bytes);
    }

    /// 未実装命令などで機械が止まった理由 (無ければ空文字列)
    pub fn trap_reason(&self) -> String {
        self.m
            .trap
            .as_ref()
            .map(|t| format!("{} @ {:04x}:{:08x}", t.reason, t.cs, t.ip))
            .unwrap_or_default()
    }

    /// 指定した仮想時間 (命令数換算) だけ進める。**1フレーム分の仕事**として呼ぶ。
    ///
    /// HLTで止まっていても抜けない — タイマ割り込みで起きるのを待つ必要があるため。
    /// アイドル中のOSは「HLTして割り込みを待つ」を繰り返している。
    ///
    /// 予算は**TSCの進み**で数える。忙しいときは1命令=1で従来どおりだが、
    /// アイドル (HLT) の早送りが飛ばした時間も含む。step の回数で数えると、
    /// 暇なときは1回でPIT1周期ぶん時間が飛ぶので、同じ予算でゲストの時計が
    /// 何百倍も速く回ってしまう
    pub fn run_slice(&mut self, instructions: f64) {
        let budget = instructions as u64;
        let start = self.m.cpu.tsc;
        while self.m.cpu.tsc.wrapping_sub(start) < budget {
            self.m.step();
            // デバッガが止めたらフレームを打ち切る。**見張っていなければ
            // この判定は真偽値1つ**なので、通常の実行には効かない
            if self.m.dbg.on && self.m.dbg.stop.is_some() {
                break;
            }
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

    /// 機械の状態をまるごと書き出す (CPU・装置・メモリ・ディスク)。
    /// JSONに束ねるのは呼び出し側の仕事で、ここは中身だけを返す
    pub fn save_state(&self) -> Vec<u8> {
        self.m.save_state()
    }

    /// 書き出した状態へ戻す
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), JsError> {
        self.m.load_state(data).map_err(|e| JsError::new(&e))
    }

    // ---------- デバッガ ----------
    //
    // 画面の向こうで動いているものを、**同じページの中で**覗けるようにする。
    // JSとの境界を細かい関数で埋めると糊が増えるので、まとまった状態は
    // **JSONの文字列1本**で渡す。毎フレームではなく人間が見る速さ (10Hz程度)
    // でしか呼ばないので、組み立ての費用は問題にならない

    /// CPUの状態をJSONで返す
    pub fn cpu_json(&self) -> String {
        use rustx86_core::cpu::*;
        let c = &self.m.cpu;
        // 保護モードでは sel<<4 は嘘の番地になる。隠しレジスタ経由で引く
        let lin = c.lin(CS, c.ip);
        let bytes: Vec<String> = (0..8)
            .map(|i| format!("{:02x}", self.m.read8(lin.wrapping_add(i))))
            .collect();
        let flags: Vec<&str> = [
            (CF, "CF"),
            (PF, "PF"),
            (AF, "AF"),
            (ZF, "ZF"),
            (SF, "SF"),
            (TF, "TF"),
            (IF, "IF"),
            (DF, "DF"),
            (OF, "OF"),
        ]
        .iter()
        .filter(|(f, _)| c.flag(*f))
        .map(|(_, n)| *n)
        .collect();
        // 現在の命令を逆アセンブルする。**幅はCSのDビットが決める** (16/32)。
        // core は逆アセンブラを持たない (無依存を貫く) ので、ここで別クレートに任せる
        let raw: Vec<u8> = (0..8).map(|i| self.m.read8(lin.wrapping_add(i))).collect();
        let bits = if c.seg_is32(CS) { 32 } else { 16 };
        let asm = rustx86_disasm::one(&raw, bits, c.ip as u64);
        // 未実装で止まっているか (panicではなく巻き戻せる停止)
        let trap = self
            .m
            .trap
            .as_ref()
            .map(|t| t.reason.replace('"', "'"))
            .unwrap_or_default();
        // 保護モードの様子。隠しレジスタは「見張るべき3本」だけ渡す
        let seg = |i: usize| {
            let h = &c.hidden[i];
            format!(
                r#"{{"sel":{},"base":{},"limit":{},"big":{}}}"#,
                c.sregs[i], h.base, h.limit, h.big
            )
        };
        format!(
            r#"{{"regs":[{}],"sregs":[{}],"ip":{},"flags":{},"flagNames":"{}",
               "bytes":"{}","asm":"{}","trap":"{}","instr":{},"executed":{},"halted":{},"lin":{},
               "pe":{},"cr0":{},"cpl":{},"pg":{},"cr2":{},"cr3":{},"gdtrBase":{},"gdtrLimit":{},"idtrBase":{},"idtrLimit":{},"trSel":{},
               "cs":{},"ds":{},"ss":{},"machine":"{}","ramMb":{}}}"#,
            c.regs
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(","),
            c.sregs
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(","),
            c.ip,
            c.flags,
            flags.join(" "),
            bytes.join(" "),
            asm.replace('\\', "\\\\").replace('"', "'"),
            trap,
            self.m.dbg.instr,
            self.m.dbg.executed,
            self.m.halted,
            lin,
            c.pe(),
            c.cr0,
            c.cpl(),
            c.cr0 & 0x8000_0000 != 0,
            c.cr2,
            c.cr3,
            c.gdtr_base,
            c.gdtr_limit,
            c.idtr_base,
            c.idtr_limit,
            c.tr_sel,
            seg(CS),
            seg(DS),
            seg(SS),
            self.m.profile.name,
            self.m.ram_bytes() / (1 << 20),
        )
    }

    /// メモリを読む。**デバッガ用なので副作用は無い**
    pub fn read_mem(&self, addr: u32, len: u32) -> Vec<u8> {
        (0..len)
            .map(|i| self.m.read8(addr.wrapping_add(i)))
            .collect()
    }

    pub fn set_break(&mut self, lin: u32) {
        self.m.dbg.break_at(lin);
    }

    pub fn watch_mem(&mut self, addr: u32) {
        self.m.dbg.watch_mem(addr);
    }

    pub fn watch_io(&mut self, port: u16, read: bool, write: bool) {
        self.m.dbg.watch_io(port, read, write);
    }

    pub fn clear_debug(&mut self) {
        self.m.dbg.clear();
    }

    /// 1命令だけ進める。止まっていても進める (割り込みで起きることがある)
    pub fn step_one(&mut self) {
        self.m.dbg.run_for(1);
        loop {
            self.m.step();
            if self.m.dbg.stop.is_some() {
                break;
            }
        }
        self.m.dbg.take_stop();
    }

    /// 止まった理由を取り出す。**取ると消える**ので、続行するとまた走り出す。
    /// 止まっていなければ空文字
    pub fn take_stop(&mut self) -> String {
        use rustx86_core::debug::Stop;
        match self.m.dbg.take_stop() {
            None => String::new(),
            Some(Stop::Break(a)) => format!("breakpoint at {a:#07x}"),
            // **書いた命令の位置まで言う**のがこの道具の要点なので、
            // どの文面にも `by CS:IP` を落とさない
            Some(Stop::WriteMem { addr, old, new, at }) => format!(
                "{addr:#07x} changed {old:#04x} -> {new:#04x} by {:04x}:{:04x}",
                at.0, at.1
            ),
            Some(Stop::WriteIo { port, val, at }) => {
                format!(
                    "wrote {val:#04x} to port {port:#06x} by {:04x}:{:04x}",
                    at.0, at.1
                )
            }
            Some(Stop::ReadIo { port, val, at }) => {
                format!(
                    "read {val:#04x} from port {port:#06x} by {:04x}:{:04x}",
                    at.0, at.1
                )
            }
            Some(Stop::Count(n)) => format!("reached instruction {n}"),
        }
    }

    /// いま止まっているか (`take_stop` と違い**消さない**)
    pub fn is_stopped(&self) -> bool {
        self.m.dbg.stop.is_some()
    }

    /// 見張っているものの一覧をJSONで
    pub fn watches_json(&self) -> String {
        let d = &self.m.dbg;
        format!(
            r#"{{"code":[{}],"mem":[{}],"ioR":[{}],"ioW":[{}],"on":{}}}"#,
            d.code
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(","),
            d.mem_write
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(","),
            d.io_read
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(","),
            d.io_write
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(","),
            d.on,
        )
    }

    /// 命令数だけ数え始める / やめる。デバッガの画面を開いている間に使う
    pub fn set_counting(&mut self, on: bool) {
        self.m.dbg.set_counting(on);
    }

    /// 足跡を残し始める
    pub fn record_trace(&mut self, n: usize) {
        self.m.dbg.record_trace(n);
    }

    /// 足跡をJSONで
    pub fn trace_json(&self) -> String {
        let bits = if self.m.cpu.seg_is32(rustx86_core::cpu::CS) {
            32
        } else {
            16
        };
        let rows: Vec<String> = self
            .m
            .dbg
            .trace
            .iter()
            .map(|s| {
                let b: Vec<String> = s.bytes.iter().map(|v| format!("{v:02x}")).collect();
                let asm = rustx86_disasm::one(&s.bytes, bits, s.ip as u64).replace('"', "'");
                format!(
                    r#"{{"i":{},"cs":{},"ip":{},"b":"{}","asm":"{}"}}"#,
                    s.instr,
                    s.cs,
                    s.ip,
                    b.join(" "),
                    asm
                )
            })
            .collect();
        format!("[{}]", rows.join(","))
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
