//! NE2000のテスト。パケットドライバ (NE2000.COM) が踏む手順を
//! そのまま踏んで、外から見える振る舞いを固定する。

use rustx86_core::Machine;

const BASE: u16 = 0x300;
const DATA: u16 = 0x310;
const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// ドライバの初期化手順そのまま。受信リングは 0x46-0x60、送信は 0x40
fn init_nic(m: &mut Machine) {
    m.io_write8(BASE, 0x21); // CR: 停止 + DMA中断 (ページ0)
    m.io_write8(BASE + 0x0E, 0x49); // DCR
    m.io_write8(BASE + 0x01, 0x46); // PSTART
    m.io_write8(BASE + 0x02, 0x60); // PSTOP
    m.io_write8(BASE + 0x03, 0x46); // BNRY
    m.io_write8(BASE + 0x0C, 0x04); // RCR: ブロードキャスト受入
    m.io_write8(BASE + 0x0D, 0x00); // TCR
    m.io_write8(BASE, 0x61); // ページ1へ
    for (i, b) in MAC.iter().enumerate() {
        m.io_write8(BASE + 1 + i as u16, *b); // PAR
    }
    m.io_write8(BASE + 0x07, 0x47); // CURR = PSTART+1
    m.io_write8(BASE, 0x22); // ページ0・開始
    m.io_write8(BASE + 0x0F, 0x03); // IMR: 受信+送信で割り込み
}

/// リモートDMAで読む: RSAR/RBCRを積んでデータポートをn回読む
fn dma_read(m: &mut Machine, addr: u16, n: usize) -> Vec<u8> {
    m.io_write8(BASE + 0x08, addr as u8);
    m.io_write8(BASE + 0x09, (addr >> 8) as u8);
    m.io_write8(BASE + 0x0A, n as u8);
    m.io_write8(BASE + 0x0B, (n >> 8) as u8);
    m.io_write8(BASE, 0x0A); // CR: リモート読み + 開始
    (0..n).map(|_| m.io_read8(DATA)).collect()
}

#[test]
fn prom_carries_the_mac_twice_and_the_signature() {
    let mut m = Machine::new();
    m.net_attach(MAC);
    // PROMはリモートDMAのアドレス0にある。各バイトが2度ずつ
    let prom = dma_read(&mut m, 0, 32);
    for (i, b) in MAC.iter().enumerate() {
        assert_eq!(prom[i * 2], *b);
        assert_eq!(prom[i * 2 + 1], *b);
    }
    // 'W' 0x57 がNE2000の名乗り
    assert_eq!(prom[14], 0x57);
    assert_eq!(prom[15], 0x57);
}

#[test]
fn transmit_hands_the_frame_to_the_outside() {
    let mut m = Machine::new();
    m.net_attach(MAC);
    init_nic(&mut m);

    // フレームをリモートDMAで送信バッファ (ページ0x40) へ書く
    let frame: Vec<u8> = (0..60u8).collect();
    m.io_write8(BASE + 0x08, 0x00); // RSAR = 0x4000
    m.io_write8(BASE + 0x09, 0x40);
    m.io_write8(BASE + 0x0A, frame.len() as u8);
    m.io_write8(BASE + 0x0B, 0x00);
    m.io_write8(BASE, 0x12); // CR: リモート書き + 開始
    for b in &frame {
        m.io_write8(DATA, *b);
    }
    // 書き終えるとリモートDMA完了 (RDC) が立つ
    assert_ne!(m.io_read8(BASE + 0x07) & 0x40, 0, "RDCが立っていない");

    // TPSR/TBCRを設定してTXP
    m.io_write8(BASE + 0x04, 0x40);
    m.io_write8(BASE + 0x05, frame.len() as u8);
    m.io_write8(BASE + 0x06, 0x00);
    m.io_write8(BASE, 0x26); // CR: 開始 + TXP

    let out = m.net_take_frames();
    assert_eq!(out, vec![frame], "送信フレームが外に出ていない");
    assert_ne!(
        m.io_read8(BASE + 0x07) & 0x02,
        0,
        "PTX (送信完了) が立っていない"
    );
    // 2度目の回収は空 (読むと消える)
    assert!(m.net_take_frames().is_empty());
}

#[test]
fn receive_lands_in_the_ring_with_header() {
    let mut m = Machine::new();
    m.net_attach(MAC);
    init_nic(&mut m);

    // ブロードキャスト宛のARPっぽい42バイト (wsslirpのARP応答と同じ長さ)
    let mut frame = vec![0xFF; 6];
    frame.extend_from_slice(&MAC);
    frame.extend_from_slice(&[0x08, 0x06]);
    frame.resize(42, 0xAB);
    assert!(m.net_inject_frame(&frame), "受信リングに入らなかった");

    // ISRに受信完了
    assert_ne!(m.io_read8(BASE + 0x07) & 0x01, 0, "PRXが立っていない");

    // CURR (ページ1) が進んでいる。60バイト+4バイトヘッダ=1ページ
    m.io_write8(BASE, 0x62); // ページ1
    let curr = m.io_read8(BASE + 0x07);
    assert_eq!(curr, 0x48, "CURRが1ページ進むはず (0x47+1)");
    m.io_write8(BASE, 0x22); // ページ0へ戻す

    // リングの先頭ページ (CURRの初期値 0x47) をリモートDMAで読むと、
    // 4バイトヘッダ + フレーム (60バイトにパディング済み) が居る
    let got = dma_read(&mut m, 0x4700, 64);
    assert_eq!(got[0], 0x01, "受信状態が正常でない");
    assert_eq!(got[1], 0x48, "次ページのリンクが違う");
    let total = got[2] as usize | (got[3] as usize) << 8;
    assert_eq!(total, 64, "ヘッダの長さ (4+60パディング) が違う");
    assert_eq!(&got[4..46], &frame[..], "フレーム本体が化けた");
    assert!(got[46..64].iter().all(|b| *b == 0), "パディングが0でない");
}

/// **束で届いても取りこぼさない。**
///
/// 外の世界 (WebSocket) はTCPの1ウィンドウを一度に落としてくる。リングは
/// 6.6KB (フル長なら4枚) しか無いので、届いた場で全部押し込もうとすると
/// 残りが消える — 実測でブラウザの受信フレームの6〜9%が消え、TCPの再送と
/// 輻輳制御でwgetの実効速度が半分以下になっていた。線 (rx_queue) で待たせ、
/// ドライバがBNRYを進めるたびに詰めれば1枚も落ちない
#[test]
fn a_burst_bigger_than_the_ring_is_not_lost() {
    let mut m = Machine::new();
    m.net_attach(MAC);
    init_nic(&mut m);

    // フル長フレーム12枚 = リングの3倍。中身の先頭バイトで通し番号を振る
    let frames: Vec<Vec<u8>> = (0..12u8)
        .map(|i| {
            let mut f = MAC.to_vec();
            f.extend_from_slice(&MAC);
            f.resize(1514, i);
            f[14] = i; // ヘッダの後ろ = 通し番号 (読み出して照合する)
            f
        })
        .collect();
    for (i, f) in frames.iter().enumerate() {
        assert!(m.net_inject_frame(f), "{i}枚目を線で待たせずに落とした");
    }

    // ドライバのように読み出す: ヘッダの「次ページ」を辿り、読み終えたら
    // BNRYを進める。12枚が順番どおり全部出てくること
    let mut page = 0x47u16; // CURRの初期値 = 最初のフレームが居るページ
    for i in 0..12u8 {
        let head = dma_read(&mut m, page << 8, 24);
        assert_eq!(head[0], 0x01, "{i}枚目の受信状態が正常でない");
        assert_eq!(head[4 + 14], i, "{i}枚目の中身が違う (順番が入れ替わった)");
        let next = head[1] as u16;
        // 読み終えたページを返す (BNRY = 次ページの1つ手前)
        let bnry = if next == 0x46 { 0x5F } else { next - 1 };
        m.io_write8(BASE + 0x03, bnry as u8);
        page = next;
    }
}

#[test]
fn foreign_unicast_is_filtered_out() {
    let mut m = Machine::new();
    m.net_attach(MAC);
    init_nic(&mut m);
    // 他人宛 (別のMAC) は落ちる
    let mut frame = vec![0x02, 0x00, 0x00, 0x00, 0x00, 0x99];
    frame.resize(60, 0);
    assert!(!m.net_inject_frame(&frame));
    assert_eq!(m.io_read8(BASE + 0x07) & 0x01, 0, "他人宛でPRXが立った");
}

#[test]
fn nic_absent_machine_sees_an_empty_slot() {
    let mut m = Machine::new();
    // 挿していなければ 0xFF (未接続ポートの既定) が返り、注入も落ちる
    assert_eq!(m.io_read8(BASE), 0xFF);
    assert!(!m.net_inject_frame(&[0u8; 60]));
    assert!(m.net_take_frames().is_empty());
}

/// `rep insw` / `rep outsw` — ストリングI/O。NE2000のデータポートは
/// これで読み書きするのが定石で、Crynwrパケットドライバの送受信の根幹。
/// 未実装だった間、DOSゲストの送信は1バイトも出なかった (実話)
#[test]
fn rep_string_io_moves_data_through_the_port() {
    let mut m = Machine::new();
    m.net_attach(MAC);

    // リモートDMA読み: PROM先頭4バイトをCPUの rep insw で 0000:8000 へ
    m.io_write8(BASE + 0x0E, 0x49); // DCR (ワード転送の顔をしておく)
    m.io_write8(BASE + 0x08, 0x00); // RSAR = 0
    m.io_write8(BASE + 0x09, 0x00);
    m.io_write8(BASE + 0x0A, 0x04); // RBCR = 4
    m.io_write8(BASE + 0x0B, 0x00);
    m.io_write8(BASE, 0x0A); // リモート読み + 開始

    #[rustfmt::skip]
    let prog: &[u8] = &[
        0xFC,                   // cld
        0xB8, 0x00, 0x00,       // mov ax,0
        0x8E, 0xC0,             // mov es,ax
        0xBF, 0x00, 0x80,       // mov di,0x8000
        0xB9, 0x02, 0x00,       // mov cx,2 (ワード2個 = 4バイト)
        0xBA, 0x10, 0x03,       // mov dx,0x310
        0xF3, 0x6D,             // rep insw
        0xF4,                   // hlt
    ];
    let mut sector = prog.to_vec();
    sector.resize(512, 0);
    sector[510] = 0x55;
    sector[511] = 0xAA;
    m.load_boot_sector(&sector).unwrap();
    m.run(1000);
    assert_eq!(
        (0..4).map(|i| m.read8(0x8000 + i)).collect::<Vec<_>>(),
        vec![MAC[0], MAC[0], MAC[1], MAC[1]],
        "rep insw がPROMを運んでいない"
    );

    // リモートDMA書き: 0000:8100 の4バイトを rep outsw でSRAM 0x4000へ
    #[rustfmt::skip]
    let prog2: &[u8] = &[
        0xFC,                   // cld
        0xB8, 0x00, 0x00,       // mov ax,0
        0x8E, 0xD8,             // mov ds,ax
        0xBE, 0x00, 0x81,       // mov si,0x8100
        0xB9, 0x02, 0x00,       // mov cx,2
        0xBA, 0x10, 0x03,       // mov dx,0x310
        0xF3, 0x6F,             // rep outsw
        0xF4,                   // hlt
    ];
    let mut sector2 = prog2.to_vec();
    sector2.resize(512, 0);
    sector2[510] = 0x55;
    sector2[511] = 0xAA;
    // 機械は新品から (使い回すと前のHLT状態やRAMクリアの作法に足を取られる)。
    // **素材とNICの設定はload_boot_sectorの後** — RAMがまっさらになるため
    let mut m = Machine::new();
    m.net_attach(MAC);
    m.load_boot_sector(&sector2).unwrap();
    for (i, b) in [0xDE, 0xAD, 0xBE, 0xEF].iter().enumerate() {
        m.write8(0x8100 + i as u32, *b);
    }
    m.io_write8(BASE + 0x08, 0x00); // RSAR = 0x4000
    m.io_write8(BASE + 0x09, 0x40);
    m.io_write8(BASE + 0x0A, 0x04);
    m.io_write8(BASE + 0x0B, 0x00);
    m.io_write8(BASE, 0x12); // リモート書き + 開始
    m.run(1000);
    let got = dma_read(&mut m, 0x4000, 4);
    assert_eq!(
        got,
        vec![0xDE, 0xAD, 0xBE, 0xEF],
        "rep outsw がSRAMへ届いていない"
    );
}

/// ISRのRSTビットの生涯: 電源投入とSTOPで立ち、**STARTで自動的に下りる**。
/// これが残っているとELKSのISRハンドラが「誰も処理しない0x80」を
/// 読み続けて無限ループする (実際にktcp起動がそれで凍った)
#[test]
fn isr_reset_bit_clears_on_start() {
    let mut m = Machine::new();
    m.net_attach(MAC);
    assert_ne!(m.io_read8(BASE + 0x07) & 0x80, 0, "電源投入直後はRSTが立つ");
    m.io_write8(BASE, 0x22); // START
    assert_eq!(m.io_read8(BASE + 0x07) & 0x80, 0, "STARTでRSTが下りる");
    m.io_write8(BASE, 0x21); // STOP
    assert_ne!(m.io_read8(BASE + 0x07) & 0x80, 0, "STOPでまた立つ");
}

#[test]
fn snapshot_roundtrip_keeps_the_nic() {
    let mut m = Machine::new();
    m.net_attach(MAC);
    init_nic(&mut m);
    let mut frame = vec![0xFF; 6];
    frame.extend_from_slice(&MAC);
    frame.resize(60, 0x5A);
    assert!(m.net_inject_frame(&frame));

    let snap = m.save_state();
    let m2 = &mut Machine::new();
    m2.load_state(&snap).expect("スナップショット復元");
    // 復元後もリングの中身とレジスタが生きている
    let got = dma_read(m2, 0x4700, 64);
    assert_eq!(&got[4..64], &frame[..], "復元後のリングが違う");
    m2.io_write8(BASE, 0x62);
    assert_eq!(m2.io_read8(BASE + 0x07), 0x48);
}

#[test]
fn page_switch_without_start_keeps_the_receiver_running() {
    // **STA/STPは状態ではなくコマンド。** Linuxの8390.cは割り込みのたびに
    // 0x20 (素のNODMA、STAもSTPも無し) を書いてページ0を選ぶ。これで受信機が
    // 止まったことにすると、割り込みの後に来たフレームを全部落とす
    // (実際にARP応答が返らなかった)
    let mut m = Machine::new();
    m.net_attach(MAC);
    init_nic(&mut m);
    m.io_write8(BASE, 0x20); // ページ切替だけ。走行状態は変えない
    let mut frame = vec![0xFF; 6];
    frame.extend_from_slice(&MAC);
    frame.resize(60, 0x11);
    assert!(m.net_inject_frame(&frame), "0x20の後も受信できること");
    m.io_write8(BASE, 0x21); // 明示のSTOP
    assert!(!m.net_inject_frame(&frame), "STOPでは受信しない");
}

/// PCI機 (32bit) での顔: 設定空間に見え、BARの窓で応え、ISAの0x300窓は閉じる
#[test]
fn pci_machine_wears_the_rtl8029_face() {
    use rustx86_core::MachineProfile;
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(4));
    m.net_attach(MAC);

    // 設定空間: スロット3に 10EC:8029 が見える
    m.io_write32(0xCF8, 0x8000_0000 | (3 << 11));
    assert_eq!(m.io_read32(0xCFC), 0x8029_10EC, "RTL8029の身元");

    // COMMANDのI/O許可を下ろしたまま触っても、誰も応えない
    assert_eq!(m.io_read8(0xC000), 0xFF, "許可前は名乗らない");

    // 許可を出すと、BARの窓 (0xC000) にDP8390のレジスタが現れる
    m.io_write32(0xCF8, 0x8000_0000 | (3 << 11) | 0x04);
    m.io_write32(0xCFC, 0x0001); // COMMAND.IO
    m.io_write8(0xC000 + 0x0E, 0x49); // DCR — 書けること
                                      // PROMは**平ら** (ne2k-pciは連続バイトをMACとして読む)
    m.io_write8(0xC000 + 0x08, 0); // RSAR
    m.io_write8(0xC000 + 0x09, 0);
    m.io_write8(0xC000 + 0x0A, 16); // RBCR
    m.io_write8(0xC000 + 0x0B, 0);
    m.io_write8(0xC000, 0x0A); // リモート読み + 開始
    let prom: Vec<u8> = (0..16).map(|_| m.io_read8(0xC000 + 0x10)).collect();
    assert_eq!(&prom[..6], &MAC, "MACは各バイト1度ずつ");
    assert_eq!((prom[14], prom[15]), (0x57, 0x57), "印は14/15に移る");

    // **ISAの0x300窓は開かない** — 同じ実体が両方の窓で応えると2枚に数えられる
    assert_eq!(m.io_read8(BASE + 0x07), 0xFF, "PCI機に0x300のカードは無い");
}

/// フロッピー起動機 (ブラウザのFreeDOS/ELKSと同じ機械) では、NICは
/// **ISAの0x300に挿さり、PROMは倍幅**であること。
///
/// この機械はPCIを積まない。積むと [`Machine::net_attach`] がPCIスロット側へ
/// 挿してISAの窓が閉じ、FreeDOSのパケットドライバからNICが消える —
/// MACが FF:FF:FF:FF:FF:FF に化けて DHCP が沈黙した (2026-08-14のデグレ)。
/// **coreのテストは16bit機 (PC_16BIT) しか見ておらず、ブラウザだけが
/// 32bitプロファイルでフロッピーを起動していたので誰も気づかなかった。**
/// ここがその見張り
#[test]
fn floppy_machine_keeps_the_nic_on_the_isa_window() {
    use rustx86_core::MachineProfile;
    let mut m = Machine::with_profile(MachineProfile::pc_floppy(16));
    m.net_attach(MAC);

    // PCIそのものが無い (設定空間は誰も応えない)
    m.io_write32(0xCF8, 0x8000_0000 | (3 << 11));
    assert_eq!(m.io_read32(0xCFC), 0xFFFF_FFFF, "フロッピー機にPCIは無い");

    // パケットドライバの手順でPROMを読む: STP → RBCR/RSAR → リモート読み
    m.io_write8(BASE, 0x21); // STP | page0
    m.io_write8(BASE + 0x0A, 32); // RBCR
    m.io_write8(BASE + 0x0B, 0);
    m.io_write8(BASE + 0x08, 0); // RSAR
    m.io_write8(BASE + 0x09, 0);
    m.io_write8(BASE, 0x0A); // リモート読み + 開始
    let prom: Vec<u8> = (0..16).map(|_| m.io_read8(BASE + 0x10)).collect();
    // ISAの8bit経路の癖で各バイトが2度ずつ並ぶ。ドライバは偶数バイトを拾う
    for (i, b) in MAC.iter().enumerate() {
        assert_eq!(prom[i * 2], *b, "PROM {}バイト目", i * 2);
        assert_eq!(prom[i * 2 + 1], *b, "倍幅の写し");
    }
    assert_eq!((prom[14], prom[15]), (0x57, 0x57), "NE2000の印は14/15");
}
