//! ディスクイメージからOSを起動する。
//!
//! ブラウザより先にCLIを作っているのは、**ここが「直す」作業だから**である。
//! どこで止まったかを素早く繰り返し見たいので、grepもdiffも効くCLIが速い。
//! UARTは `feed()` / `tx` というバイト列の口を持っているので、
//! シェルが出た後にブラウザへ繋いでもコアの作り直しは起きない。
//!
//! ```text
//! cargo run --release --example boot -- images/fd1440.img
//! ```

use rustx86_core::Machine;

fn main() {
    let path = std::env::args().nth(1).expect("usage: boot <disk.img>");
    let max: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000_000);

    let image = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut m = Machine::new();
    m.boot_from_disk(image).expect("boot");

    // 画面に文字列が出るまで走らせ、出たらキーを打つ。
    //
    // 「一定命令数だけ回してから打つ」ではなく**プロンプトを見てから打つ**のは、
    // 起動にかかる時間が環境で変わるためである。人間が画面を見て打つのと同じ手順。
    /// **もう二度と動き出さない**と言い切れる状態か。
    ///
    /// HLTしているだけでは足りない。OSはキー入力を待つときもHLTするので、
    /// **叩けば起きる**かどうかを見なければならない。打ったキーがまだ
    /// 8042の待ち行列に残っていれば、次のtickでIRQ1が上がって目を覚ます。
    ///
    /// ここを「halted かつ PIT停止」だけで判定していたため、キーを打った直後に
    /// 一度もstepせず諦めていた。**入力を渡した本人が、渡した直後に見捨てていた**。
    fn stuck(m: &Machine) -> bool {
        m.halted
            && m.pending_irq.is_none()
            && !m.devices.pit.counters[0].running
            && !m.devices.keyboard.has_data()
    }

    fn run_until<'a>(m: &mut Machine, needle: &str, budget: u64) -> bool {
        for _ in 0..budget {
            if stuck(m) {
                break;
            }
            m.step();
            // 画面を毎命令組み立ててはいけない。1命令ごとに80x25文字のStringを
            // 作ることになり、起動が数百倍遅くなる (実際にやって390秒かかった)。
            // Tier 2a で入れた dirty フラグで、**書き換わったときだけ**見る
            if m.take_vram_dirty() && m.text_screen_string().contains(needle) {
                return true;
            }
        }
        false
    }

    // 引数は **「この文字列が出たら」「これを打つ」の対**。
    // 何命令目で打つかではなく画面を見てから打つのは、起動時間が環境で変わるためで、
    // 人間が画面を見て打つのと同じ手順になっている。
    //
    // 引数なしのときは ELKS の既定 (login: を待って root と打つ)。
    //
    // ```text
    // boot images/fd14boot.img 400000000 "Select from Menu" '\n' 'C:\>' 'dir\n'
    // ```
    fn unescape(s: &str) -> String {
        s.replace("\\n", "\n").replace("\\r", "\r").replace("\\t", "\t")
    }
    let args: Vec<String> = std::env::args().skip(3).collect();
    let script: Vec<(String, String)> = if args.is_empty() {
        vec![("login:".into(), "root\n".into())]
    } else {
        args.chunks(2)
            .map(|c| (c[0].clone(), unescape(c.get(1).map(String::as_str).unwrap_or(""))))
            .collect()
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut n = 0u64;
        for (wait_for, send) in &script {
            if !run_until(&mut m, wait_for, max) {
                eprintln!("[{wait_for:?} が出ないまま打ち切り]");
                return n;
            }
            eprintln!("[{wait_for:?} を検出 → {send:?} を入力]");
            // `sc:3f,bf` の形なら**生のスキャンコードを流す**。
            // ファンクションキーのように文字を持たないキーを送るための口で、
            // DOSの F5 (CONFIG/AUTOEXEC を飛ばす) を打つのに要る
            if let Some(list) = send.strip_prefix("sc:") {
                let codes: Vec<u8> = list
                    .split(',')
                    .filter_map(|s| u8::from_str_radix(s.trim(), 16).ok())
                    .collect();
                m.devices.keyboard.feed(&codes);
            } else {
                // **1文字ずつ、間を空けて打つ。**
                //
                // まとめて流し込むと、BIOSの待ち行列 (16枠) がゲストの読み出しより
                // 速く埋まって取りこぼす。人間は毎秒10文字ほどしか打たないので、
                // 実機ではこの詰まりが起きない。**打つ側が速すぎたのが原因**で、
                // エミュレータ側のバグではなかった
                for ch in send.chars() {
                    m.devices.keyboard.type_ascii(&ch.to_string());
                    for _ in 0..1_000_000 {
                        if stuck(&m) {
                            break;
                        }
                        m.step();
                        n += 1;
                    }
                }
            }
            // 打った内容が処理されるまで回す
            for _ in 0..250_000_000 {
                if stuck(&m) {
                    break;
                }
                m.step();
                n += 1;
            }
        }
        n
    }));

    // 止まった理由より先に、**ゲストが何を出したか**を見せる。
    // 大抵はそこに手がかりが書いてある
    let uart = m.devices.uart.tx_string();
    if !uart.is_empty() {
        println!("--- シリアル (COM1) ---\n{uart}");
    }
    let con = m.console_string();
    if !con.is_empty() {
        println!("--- BIOS画面 (INT 10h) ---\n{con}");
    }
    let screen = m.text_screen_string();
    if !screen.is_empty() {
        println!("--- テキストVRAM ---\n{screen}\n--- (ここまで) ---");
    }

    // **色だけで描かれた絵**は文字を見るだけでは消える。
    // テトリスのブロックのように「背景色を付けた空白」で描くソフトがあるので、
    // 背景が黒でないセルは塗りつぶしとして見せる (実際これで一度騙された)
    {
        let v = m.text_vram();
        let painted: String = (0..25)
            .map(|row| {
                let line: String = (0..80)
                    .map(|col| {
                        let i = (row * 80 + col) * 2;
                        let (ch, attr) = (v[i], v[i + 1]);
                        if ch == b' ' && (attr >> 4) & 7 != 0 {
                            '█' // 色だけのセル
                        } else {
                            rustx86_core::cp437::to_char(ch)
                        }
                    })
                    .collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        if painted.trim() != m.text_screen_string().trim() {
            println!("--- テキストVRAM (色付きセルを塗って表示) ---\n{}\n--- (ここまで) ---", painted.trim_end());
        }
    }

    // カーソルの居場所は「画面が空に見える」ときの手がかりになる。
    // 書いた先が見ている場所と違う、という取り違えがすぐ分かる
    let (crow, ccol) = m.cursor_pos();
    println!("--- カーソル: 行{crow} 桁{ccol} ---");
    if !m.prefixed_ops.is_empty() {
        let list: Vec<String> = m.prefixed_ops.iter().map(|o| format!("{o:#04x}")).collect();
        println!("--- 0x66 を付けて実行されたオペコード ---\n  {}", list.join(" "));
    }

    {
        let (head, tail) = (m.read16(0x41A), m.read16(0x41C));
        println!(
            "--- BIOSキー待ち行列: head={head:#06x} tail={tail:#06x} ({}) / 修飾={:#04x} / 8042残り={} ---",
            if head == tail { "空 = 読まれた" } else { "残っている = 誰も読んでいない" },
            m.read8(0x417),
            m.devices.keyboard.has_data(),
        );
        let c = &m.devices.pit.counters[0];
        println!(
            "--- PIT カウンタ0: running={} mode={} reload={:#06x} ({:.1} Hz) ---",
            c.running,
            c.mode,
            c.reload,
            m.devices.pit.irq0_hz()
        );
    }

    println!("--- 状態 ---");
    match result {
        Ok(n) => println!("{n} 命令 実行。halted={}", m.halted),
        Err(_) => println!("panic で停止 (上のメッセージ参照)"),
    }
    println!(
        "CS:IP={:04x}:{:04x} SP={:04x} flags={:04x}",
        m.cpu.sregs[rustx86_core::cpu::CS],
        m.cpu.ip,
        m.cpu.regs[rustx86_core::cpu::SP] as u16,
        m.cpu.flags
    );

    // ベクタごとの回数と初出。実OSのデバッグはここから始まる
    let fired: Vec<String> = (0..256)
        .filter(|v| m.int_counts[*v] > 0)
        .map(|v| {
            let (cs, ip) = m.int_first[v];
            format!("{v:#04x}×{} ({cs:04x}:{ip:04x})", m.int_counts[v])
        })
        .collect();
    if !fired.is_empty() {
        println!("割り込み (ベクタ×回数、初出): {}", fired.join("  "));
    }
    if !m.int_recent.is_empty() {
        let s: Vec<String> = m
            .int_recent
            .iter()
            .map(|(v, cs, ip)| format!("{v:#04x}@{cs:04x}:{ip:04x}"))
            .collect();
        println!("直近の割り込み: {}", s.join(" → "));
    }

    // 初出位置の命令バイト。「本物の例外」か「ゴミを実行した」かはここで分かる
    for v in [0u8, 1, 3, 4, 6] {
        if m.int_counts[v as usize] > 0 {
            let (cs, ip) = m.int_first[v as usize];
            let a = rustx86_core::cpu::operand::linear(cs, ip);
            let before: Vec<String> =
                (1..=6).rev().map(|i| format!("{:02x}", m.read8(a - i))).collect();
            let b: Vec<String> = (0..8).map(|i| format!("{:02x}", m.read8(a + i))).collect();
            println!(
                "INT {v:#04x} 初出 {cs:04x}:{ip:04x}  直前[{}]  IP以降[{}]",
                before.join(" "),
                b.join(" ")
            );
        }
    }

    if let Some((vec, cs, ip)) = m.first_fault {
        let a = rustx86_core::cpu::operand::linear(cs, ip);
        let b: Vec<String> = (0..8).map(|i| format!("{:02x}", m.read8(a + i))).collect();
        println!(
            "最初のCPU例外: INT {vec:#04x} @ {cs:04x}:{ip:04x}  命令バイト: {}",
            b.join(" ")
        );
    }

    // 止まった/回り続けている位置の周辺を見せる。
    // 逆アセンブラは持っていないので生バイトだが、オペコード表と突き合わせれば読める
    let base = rustx86_core::cpu::operand::linear(m.cpu.sregs[rustx86_core::cpu::CS], m.cpu.ip);
    let dump: Vec<String> = (0..16)
        .map(|i| format!("{:02x}", m.read8(base.wrapping_add(i))))
        .collect();
    println!("CS:IP の命令バイト: {}", dump.join(" "));
    let back: Vec<String> = (1..=8)
        .rev()
        .map(|i| format!("{:02x}", m.read8(base.wrapping_sub(i))))
        .collect();
    println!("直前8バイト        : {}", back.join(" "));

    // 未接続ポートは「ゲストが何を探そうとしたか」の記録になる。
    // 装置を足す順番を決める手がかり
    if !m.unhandled_io.is_empty() {
        let list: Vec<String> = m
            .unhandled_io
            .iter()
            .take(24)
            .map(|p| format!("{p:#06x}"))
            .collect();
        println!(
            "触られた未接続ポート ({}件): {}",
            m.unhandled_io.len(),
            list.join(" ")
        );
    }
}
