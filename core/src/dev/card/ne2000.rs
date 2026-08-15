//! NE2000 — **ISAの基板に載ったDP8390**。
//!
//! Novellが1987年に出した安物カード。あまりに安くて全メーカーがレジスタ配置ごと
//! 真似た結果、「NE2000互換」がISA時代のネットワークの共通語になった。
//!
//! 基板の都合はPROMの並べ方ひとつである。**ISAの8bitデータ経路では
//! 各バイトが2度ずつ並ぶ** — 16bitカードは偶数バイトしか読まないので、
//! 倍幅で置くのが慣例だった。素子 ([`Dp8390`]) はこの事情を知らない。
//!
//! 窓の番地 (0x300) とIRQは基板ではなく**機械の配線**の話なので、
//! [`bus`](crate::mem::bus) 側が持つ (実機ならジャンパで決めるところ)。

use crate::dev::chip::Dp8390;

/// ISAのNE2000カードを1枚組み立てる。
pub fn build(mac: [u8; 6]) -> Dp8390 {
    let mut nic = Dp8390::new(mac);
    let mut prom = [0u8; 32];
    for (i, b) in mac.iter().enumerate() {
        prom[i * 2] = *b;
        prom[i * 2 + 1] = *b;
    }
    // 末尾の 0x57 'W' はドライバのNE2000判定の印。倍幅なので2箇所ずつ置く
    for i in [14, 15, 28, 29, 30, 31] {
        prom[i] = 0x57;
    }
    nic.write_prom(&prom);
    nic
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **PROMは倍幅**。ドライバはここを読んでMACを知り、'W' で素性を確かめる
    #[test]
    fn prom_carries_the_mac_twice_and_the_signature() {
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        let nic = build(mac);
        let prom = nic.prom();
        for (i, b) in mac.iter().enumerate() {
            assert_eq!(prom[i * 2], *b);
            assert_eq!(prom[i * 2 + 1], *b);
        }
        assert_eq!(prom[14], 0x57, "NE2000の名乗り 'W'");
        assert_eq!(prom[15], 0x57);
    }
}
