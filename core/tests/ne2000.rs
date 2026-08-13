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
