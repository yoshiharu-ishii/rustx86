//! ディスクイメージからOSを起動する。
//!
//! ブラウザより先にCLIを作っているのは、**ここが「直す」作業だから**である。
//! どこで止まったかを素早く繰り返し見たいので、grepもdiffも効くCLIが速い。
//! UARTは `feed()` / `tx` というバイト列の口を持っているので、
//! シェルが出た後にブラウザへ繋いでもコアの作り直しは起きない。
//!
//! ```text
//! cargo run --release --example boot -- images/fd2880.img
//! ```

use rustx86_core::Machine;

fn main() {
    let path = std::env::args().nth(1).expect("usage: boot <disk.img>");
    // 命令数の上限。**使う人が当てる数字ではなく、暴走を止める番人**である。
    //
    // 以前は既定 5000万で、FreeDOSを動かすには9億と手で渡す必要があった。
    // その9億に根拠は無く、試して決めた当てずっぽうだった。**道具の側がファジー**で、
    // 足りないと「合図が出ないまま打ち切り」になり、エミュレータのバグと
    // 見分けがつかない。
    //
    // 本当に止まったかどうかは [`stuck`] が判定できるので、上限は
    // 「何かの拍子に無限に回り続けるのを防ぐ」ためだけに置けばよい。
    // 既定を十分大きく取り、代わりに**実際に何命令かかったかを毎回表示する**。
    // そうすれば当てずっぽうの数字が測った数字に変わる。
    const DEFAULT_MAX: u64 = 5_000_000_000;
    /// 最後の手順の後、画面が**これ以上書き換わらなくなった**と見なすまでの命令数。
    /// 待つ合図が無いのはここだけなので、時間ではなく**画面の静けさ**で打ち切る
    const QUIET: u64 = 5_000_000;
    /// それでも止まらないとき (時計で描き変わり続けるゲームなど) の上限
    const SETTLE_MAX: u64 = 250_000_000;
    let max: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX);

    let image = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    // HDD=path で C: (INT 13h ドライブ 0x80) を挿す — DOOM のような、フロッピーに
    // 載らない DOS ソフトの器。そのときは**ブラウザと同じ 386 の機械** (pc_floppy、
    // 16MB) にする — Machine::new() は PC/XT (8086) で、DOS/16M が「386 が要る」と言う
    let hdd = std::env::var("HDD").ok();
    // ISO 9660 (セクタ 16 に CD001) なら El Torito で起動 — 中身は Linux のことが
    // 多いので 128MB の 386 機で
    let is_iso = image.len() > 0x8006 && &image[0x8001..0x8006] == b"CD001";
    let mut m = if is_iso {
        Machine::with_profile(rustx86_core::MachineProfile::pc_32bit(
            std::env::var("RAM_MB")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(128),
        ))
    } else if hdd.is_some() {
        Machine::with_profile(rustx86_core::MachineProfile::pc_floppy(16))
    } else {
        Machine::new()
    };
    // SNAPSHOT_LOAD=path: 控えから戻して続きをやる (DSL のログインまで 10 分、を 0 にする)。
    // CD の像は控えに入っていないので第1引数の ISO を挿し直す。
    // SNAPSHOT_SAVE=path: 手順を終えたところで控えを書く
    if let Ok(p) = std::env::var("SNAPSHOT_LOAD") {
        let data = std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}"));
        m.load_state(&data).expect("snapshot");
        if m.cd_wanted() {
            m.cd_attach(image);
        }
        eprintln!("[boot] {p} から復元");
    } else if is_iso {
        m.boot_from_iso(image).expect("El Torito");
    } else {
        m.boot_from_disk(image).expect("boot");
    }
    if let Some(p) = hdd {
        let img = std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}"));
        m.hdd_attach(img).expect("hdd");
    }
    // NET=1 で NIC を挿す (PCI 機なら RTL8029、16bit 機なら ISA の NE2000)。ブラウザと同じ MAC
    if std::env::var("NET").is_ok() {
        m.net_attach([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    }

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
    // 「機械が止まった」= HLT 中で、起こす当てが何も無い。**PIC の挙手も当て**に数える —
    // ワンショットの PIT (Linux の NOHZ、mode 4) は鳴った瞬間に止まるので、
    // 「PIT が動いていない」だけで死んだと見ると、IRR に IRQ0 が立ったままの機械を
    // 捨ててしまう (DSL 2024 で実際にそうなった、2026-08-23)
    fn stuck(m: &Machine) -> bool {
        m.halted
            && m.pending_irq.is_none()
            && !m.devices.pit.counters[0].running
            && !m.devices.pic[0].has_pending()
            && !m.devices.pic[1].has_pending()
            && !m.devices.keyboard.has_data()
    }

    /// 画面に `needle` が出るまで走らせ、**かかった命令数**を返す。
    /// 出ないまま上限に達したか本当に止まったら `None`
    fn run_until(m: &mut Machine, needle: &str, budget: u64) -> Option<u64> {
        for i in 0..budget {
            if stuck(m) {
                return None;
            }
            m.step();
            // 画面を毎命令組み立ててはいけない。1命令ごとに80x25文字のStringを
            // 作ることになり、起動が数百倍遅くなる (実際にやって390秒かかった)。
            // Tier 2a で入れた dirty フラグで、**書き換わったときだけ**見る
            if m.take_vram_dirty() && m.text_screen_string().contains(needle) {
                return Some(i + 1);
            }
        }
        None
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
        s.replace("\\n", "\n")
            .replace("\\r", "\r")
            .replace("\\t", "\t")
    }
    let args: Vec<String> = std::env::args().skip(3).collect();
    let script: Vec<(String, String)> = if args.is_empty() {
        vec![("login:".into(), "root\n".into())]
    } else {
        args.chunks(2)
            .map(|c| {
                (
                    c[0].clone(),
                    unescape(c.get(1).map(String::as_str).unwrap_or("")),
                )
            })
            .collect()
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut n = 0u64;
        for (i, (wait_for, send)) in script.iter().enumerate() {
            let Some(cost) = run_until(&mut m, wait_for, max) else {
                // **止まったのか、上限が足りなかったのかを言い分ける。**
                // 混ぜると「エミュレータのバグ」に見えてしまう
                if stuck(&m) {
                    eprintln!("[{wait_for:?} を待っている間に機械が止まった]");
                } else {
                    eprintln!(
                        "[{wait_for:?} が {max} 命令では出なかった。                         第2引数で上限を増やせる]"
                    );
                }
                return n;
            };
            n += cost;
            eprintln!("[{wait_for:?} を検出 ({cost} 命令) → {send:?} を入力]");
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
            // **打った後に固定で回さない。**
            //
            // 以前はここで2.5億命令ぶん回していたが、これも当てずっぽうの数字だった。
            // しかも実際の仕事がここで終わってしまい、次の `run_until` が
            // 「1命令で検出」と報告するので、**どこに時間がかかっているかが
            // 見えなくなっていた**。待つのは次の合図に任せる。
            //
            // 最後の手順の後だけは待つ相手が居ないので、画面が落ち着くまで回す
            if i + 1 == script.len() {
                let mut quiet = 0u64;
                for _ in 0..SETTLE_MAX {
                    if stuck(&m) || quiet >= QUIET {
                        break;
                    }
                    m.step();
                    n += 1;
                    // 書き換わったら数え直す。止まったら静けさが積み上がる
                    quiet = if m.take_vram_dirty() { 0 } else { quiet + 1 };
                }
            }
        }
        n
    }));

    if let Ok(p) = std::env::var("SNAPSHOT_SAVE") {
        let data = m.save_state();
        match std::fs::write(&p, &data) {
            Ok(()) => eprintln!("[boot] 控えを書いた: {p} ({} バイト)", data.len()),
            Err(e) => eprintln!("[boot] 控えを書けない: {e}"),
        }
    }

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
            println!(
                "--- テキストVRAM (色付きセルを塗って表示) ---\n{}\n--- (ここまで) ---",
                painted.trim_end()
            );
        }
    }

    // カーソルの居場所は「画面が空に見える」ときの手がかりになる。
    // 書いた先が見ている場所と違う、という取り違えがすぐ分かる
    let (crow, ccol) = m.cursor_pos();
    println!("--- カーソル: 行{crow} 桁{ccol} ---");
    {
        // 3 = 80x25 カラーテキスト。それ以外を要求されていたら、
        // 画面が白いのは「描いていない」のではなく「描く先が無い」
        let other: Vec<String> = m
            .video_modes
            .iter()
            .filter(|v| **v != 0x03 && **v != 0x02 && **v != 0x07)
            .map(|v| format!("{v:#04x}"))
            .collect();
        if !other.is_empty() {
            println!(
                "--- テキスト以外のビデオモードを要求された: {} (グラフィックスは未実装 → Tier 6) ---",
                other.join(" ")
            );
        }
    }

    if !m.prefixed_ops.is_empty() {
        let list: Vec<String> = m.prefixed_ops.iter().map(|o| format!("{o:#04x}")).collect();
        println!(
            "--- 0x66 を付けて実行されたオペコード ---\n  {}",
            list.join(" ")
        );
    }

    {
        // **割り込みを誰が持っているか。** OSが乗っ取ったベクタはBIOSへ来ない。
        // 「BIOSを直したのに効かない」ときは、たいてい相手が自分で持っている
        let owner = |v: u32| {
            let (seg, off) = (m.read16(v * 4 + 2), m.read16(v * 4));
            let who = if seg == rustx86_core::BIOS_SEG {
                "BIOS"
            } else {
                "ゲスト"
            };
            format!("{v:#04x}={seg:04x}:{off:04x}({who})")
        };
        println!(
            "--- 割り込みベクタの持ち主: {} {} {} {} ---",
            owner(0x08),
            owner(0x09),
            owner(0x10),
            owner(0x16)
        );
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
        // LFB_DUMP=path: 画面の画素 (efifb / Bochs VGA) を PPM に落とす (X の目視確認用)
        if let (Ok(path), Some(l)) = (std::env::var("LFB_DUMP"), m.lfb) {
            let fb = m.lfb_frame();
            let mut ppm = format!("P6\n{} {}\n255\n", l.width, l.height).into_bytes();
            if l.bpp == 32 {
                if m.lfb_xrgb {
                    ppm.extend(fb.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]));
                } else {
                    ppm.extend(fb.chunks_exact(4).flat_map(|p| [p[1], p[2], p[3]]));
                }
            } else {
                ppm.extend_from_slice(fb);
            }
            match std::fs::write(&path, &ppm) {
                Ok(()) => eprintln!("[boot] LFB {}x{} → {path}", l.width, l.height),
                Err(e) => eprintln!("[boot] LFB を書けない: {e}"),
            }
        }
        eprintln!(
        "--- PIC0 imr={:#04x} irr={:#04x} isr={:#04x} / PIC1 imr={:#04x} irr={:#04x} isr={:#04x} / pending_irq={:?} ---",
        m.devices.pic[0].imr, m.devices.pic[0].irr, m.devices.pic[0].isr,
        m.devices.pic[1].imr, m.devices.pic[1].irr, m.devices.pic[1].isr, m.pending_irq
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
        m.cpu.eflags()
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
            let a = rustx86_core::cpu::operand::linear(cs, ip as u16);
            let before: Vec<String> = (1..=6)
                .rev()
                .map(|i| format!("{:02x}", m.read8(a - i)))
                .collect();
            let b: Vec<String> = (0..8).map(|i| format!("{:02x}", m.read8(a + i))).collect();
            println!(
                "INT {v:#04x} 初出 {cs:04x}:{ip:04x}  直前[{}]  IP以降[{}]",
                before.join(" "),
                b.join(" ")
            );
        }
    }

    if let Some((vec, cs, ip)) = m.first_fault {
        let a = m.cpu.lin(rustx86_core::cpu::CS, ip);
        let b: Vec<String> = (0..8).map(|i| format!("{:02x}", m.read8(a + i))).collect();
        println!(
            "最初のCPU例外: INT {vec:#04x} @ {cs:04x}:{ip:04x}  命令バイト: {}",
            b.join(" ")
        );
    }

    // 止まった/回り続けている位置の周辺を見せる。
    // 逆アセンブラは持っていないので生バイトだが、オペコード表と突き合わせれば読める
    let base = m.cpu.lin(rustx86_core::cpu::CS, m.cpu.ip);
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
