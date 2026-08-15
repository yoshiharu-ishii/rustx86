//! virtio-blk — **ディスクの最初の口** (Red Hat 1AF4:1001)。
//!
//! 素子は [`VirtioPci`](crate::dev::chip::VirtioPci) (リングとレジスタ窓)。
//! この基板が持つのは**ブロック要求の解釈**と設定空間の名乗りだけである。
//!
//! ## 要求の形 (1つの鎖に3役が乗る)
//!
//! ```text
//!   [ヘッダ 16B (装置は読む)] [データ ×n] [status 1B (装置が書く)]
//!    type u32 | 予約 u32 | sector u64
//! ```
//!
//! type 0=読み 1=書き 4=flush 8=装置ID。sectorは**常に512バイト単位**
//! (実デバイスの4Kセクタ等はゲスト側が抽象する — エミュレータは楽をもらう)。
//!
//! ## なぜATAではなくこれが先か
//!
//! ATA PIOは1セクタ512BをINSW 256回=I/Oトラップ256回で運ぶ。ここでは記述子に
//! ゲスト物理アドレスが載っているので**ホストはmemcpyするだけ**。互換の相手
//! (ReactOS・純正UNIX) が来たらATAを足すが、速さはこちらで取る
//! ([roadmap](../../../../docs/roadmap.md) の決定)。

use crate::bus::pci::{Bar, PciFunction};
use crate::dev::chip::VirtioPci;

/// ブロック装置が挿さるスロット (NICの隣)
pub const BLK_SLOT: usize = 4;

/// I/O窓の番地 (firmwareが配ったことにする値)。**窓の幅=64に整列**
pub const BLK_IO_BASE: u32 = 0xC040;

/// 割り込み線。NICの3の隣で空いている5 (実機のIRQ5もサウンドか空きが相場)
pub const IRQ_BLK: u8 = 5;

/// キューの段数。128段 = 表3枚で2ページ。ドライバはこれより深くできない
const QSIZE: u16 = 128;

/// 要求ヘッダのtype
mod req {
    pub const IN: u32 = 0; // ディスク → ゲスト (読み)
    pub const OUT: u32 = 1; // ゲスト → ディスク (書き)
    pub const FLUSH: u32 = 4;
    pub const GET_ID: u32 = 8;
}

/// statusバイト (鎖の末尾に装置が書く)
mod status {
    pub const OK: u8 = 0;
    pub const IOERR: u8 = 1;
    pub const UNSUPP: u8 = 2;
}

/// virtio-blkカード1枚 — 素子 + ディスクの中身。
///
/// 中身を`Vec<u8>`で丸ごと持つのはフロッピー ([`Disk`](crate::Disk)) と同じ流儀。
/// メモリに収まる大きさ (数十MB) で始め、あふれる相手が来たら考える
pub struct VirtioBlk {
    pub vio: VirtioPci,
    pub image: Vec<u8>,
}

impl VirtioBlk {
    /// カードを組む。容量 (512Bセクタ数) は**設定空間で名乗る** —
    /// ゲストはこの8バイトを読んで /dev/vda の大きさを知る
    pub fn new(image: Vec<u8>) -> Self {
        let sectors = (image.len() as u64) / 512;
        Self {
            vio: VirtioPci::new(QSIZE, sectors.to_le_bytes().to_vec()),
            image,
        }
    }

    /// 呼び鈴が鳴っていたら、袋を全部さばく。
    /// `note_write(番地, 長さ)` はゲストRAMへ書いた場所の申告先 —
    /// **DMAは自己書き換え検出の横を通る**ので、書いた場所を必ず知らせる
    /// (dcacheがコードの写しを控えたページなら捨ててもらう)。
    /// 戻り値は割り込みを上げるべきか
    pub fn process(&mut self, ram: &mut [u8], mut note_write: impl FnMut(u32, u32)) -> bool {
        let mut irq = false;
        while let Some((head, segs)) = self.vio.pop_avail(ram) {
            let written = self.serve(ram, &segs, &mut note_write);
            irq |= self.vio.push_used(ram, head, written);
        }
        irq
    }

    /// 鎖1本ぶんの要求をさばく。返すのは装置がゲストRAMへ書いたバイト数
    fn serve(
        &mut self,
        ram: &mut [u8],
        segs: &[crate::dev::chip::virtio::Seg],
        note_write: &mut impl FnMut(u32, u32),
    ) -> u32 {
        // 形の検査: 先頭16B (読み) + 末尾1B以上 (書き)。形が崩れていたら
        // statusすら書けないかもしれないが、書ける位置にあれば UNSUPP を返す
        let (header, rest) = match segs.split_first() {
            Some((h, r)) if h.len >= 16 && !h.write && !r.is_empty() => (h, r),
            _ => return 0, // 袋が壊れている — usedには積むが何も書かない
        };
        let (tail, data) = rest.split_last().unwrap(); // rest非空は上で確認済み

        // ヘッダは写しで読む (以降ramを書き換えるので借用を残さない)
        let mut hdr = [0u8; 16];
        let Some(src) = ram.get(header.addr as usize..header.addr as usize + 16) else {
            return 0; // ヘッダがRAMの外 — 袋ごと不成立
        };
        hdr.copy_from_slice(src);
        let rtype = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
        let mut at = (sector * 512) as usize;

        let mut moved: u32 = 0; // 装置がゲストRAMへ書いた量 (usedのlenに載る)
        let code = match rtype {
            req::IN => 'io: {
                for s in data {
                    let (a, n) = (s.addr as usize, s.len as usize);
                    // 読みの結果を置く袋なのに読み専用 — 形が変
                    if !s.write {
                        break 'io status::UNSUPP;
                    }
                    let (Some(src), Some(dst)) =
                        (self.image.get(at..at + n), ram.get_mut(a..a + n))
                    else {
                        break 'io status::IOERR; // ディスクの外 or RAMの外
                    };
                    dst.copy_from_slice(src);
                    note_write(s.addr, s.len);
                    at += n;
                    moved += s.len;
                }
                status::OK
            }
            req::OUT => 'io: {
                for s in data {
                    let (a, n) = (s.addr as usize, s.len as usize);
                    if s.write {
                        break 'io status::UNSUPP;
                    }
                    let (Some(src), Some(dst)) =
                        (ram.get(a..a + n), self.image.get_mut(at..at + n))
                    else {
                        break 'io status::IOERR;
                    };
                    dst.copy_from_slice(src);
                    at += n;
                }
                status::OK
            }
            // 中身はメモリなので、flushは「はい」と言うだけで嘘にならない
            req::FLUSH => status::OK,
            req::GET_ID => 'io: {
                // 20バイトの名札。ゲストでは /sys/block/vda/serial に出る
                let mut id = [0u8; 20];
                id[..12].copy_from_slice(b"rustx86-disk");
                let Some(s) = data.first().filter(|s| s.write) else {
                    break 'io status::UNSUPP;
                };
                let n = (s.len as usize).min(20);
                let Some(dst) = ram.get_mut(s.addr as usize..s.addr as usize + n) else {
                    break 'io status::IOERR;
                };
                dst.copy_from_slice(&id[..n]);
                note_write(s.addr, n as u32);
                moved += n as u32;
                status::OK
            }
            _ => status::UNSUPP,
        };
        // statusは最後に1回だけ書く (置き場が書ける袋であることを確かめて)
        if tail.write && tail.len >= 1 {
            if let Some(b) = ram.get_mut(tail.addr as usize) {
                *b = code;
                note_write(tail.addr, 1);
                moved += 1;
            }
        }
        moved
    }

    pub fn save(&self, w: &mut crate::snapshot::Writer) {
        self.vio.save(w);
        // 中身も丸ごと。ゲストが書いたセクタを失うと復元後の世界が矛盾する
        // (フロッピーのDiskと同じ判断。RLEなので空きは潰れる)
        w.rle(&self.image);
    }

    pub fn load(r: &mut crate::snapshot::Reader) -> Result<Self, String> {
        let vio = VirtioPci::load(r)?;
        let image = r.rle()?;
        Ok(Self { vio, image })
    }
}

/// 設定空間の名乗り。**legacyの約束**: 装置ID 0x1001 (0x1000+ID群)、
/// リビジョン0、サブシステムIDが本当の型番 (2=ブロック)
pub fn pci_function(irq_line: u8) -> PciFunction {
    // class 01 = ストレージ、subclass 00 = SCSI (QEMUのvirtio-blkと同じ名乗り。
    // Linuxはclassでは選ばずベンダ/装置IDで選ぶので、ここは慣習に合わせる)
    PciFunction::new(0x1AF4, 0x1001, 0x01, 0x00, 0x00)
        .with_bar(0, Bar { size: 64, io: true }, BLK_IO_BASE)
        .with_irq(irq_line, 1) // INTA#
        // legacyではサブシステムIDが「何の装置か」を名乗る (2 = block)
        .with_subsystem(0x1AF4, 0x0002)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::pci::reg;
    use crate::dev::chip::virtio::reg as vreg;

    /// リング一式をRAMに組む小さなドライバ (テスト用のゲスト役)。
    /// 実物のドライバと同じ順で叩く: STATUS → PFN → 記述子 → avail → 呼び鈴
    struct FakeDriver {
        qsize: u64,
        base: u64,
        avail_count: u16,
    }

    impl FakeDriver {
        fn new(blk: &mut VirtioBlk, base_page: u32) -> Self {
            // ドライバの起動列 (ACKNOWLEDGE=1 → DRIVER=2 → PFN → DRIVER_OK=4)
            blk.vio.write(vreg::STATUS, 1);
            blk.vio.write(vreg::STATUS, 3);
            for (i, b) in base_page.to_le_bytes().iter().enumerate() {
                blk.vio.write(vreg::QUEUE_PFN + i as u16, *b);
            }
            blk.vio.write(vreg::STATUS, 7);
            Self {
                qsize: 128,
                base: u64::from(base_page) << 12,
                avail_count: 0,
            }
        }

        fn desc(&self, ram: &mut [u8], i: u64, addr: u32, len: u32, flags: u16, next: u16) {
            let d = (self.base + i * 16) as usize;
            ram[d..d + 8].copy_from_slice(&u64::from(addr).to_le_bytes());
            ram[d + 8..d + 12].copy_from_slice(&len.to_le_bytes());
            ram[d + 12..d + 14].copy_from_slice(&flags.to_le_bytes());
            ram[d + 14..d + 16].copy_from_slice(&next.to_le_bytes());
        }

        /// 3節の鎖 (ヘッダ→データ→status) を積んで呼び鈴を鳴らす
        #[allow(clippy::too_many_arguments)] // 実物の要求の欄がこの数ある
        fn submit(
            &mut self,
            blk: &mut VirtioBlk,
            ram: &mut [u8],
            rtype: u32,
            sector: u64,
            data_at: u32,
            len: u32,
            data_write: bool,
        ) {
            // ヘッダは0x500に置く決め (テスト内の決まりごと)
            ram[0x500..0x504].copy_from_slice(&rtype.to_le_bytes());
            ram[0x508..0x510].copy_from_slice(&sector.to_le_bytes());
            self.desc(ram, 0, 0x500, 16, 1, 1); // NEXT
            self.desc(ram, 1, data_at, len, if data_write { 3 } else { 1 }, 2);
            self.desc(ram, 2, 0x7F0, 1, 2, 0); // status: WRITE
            let avail = (self.base + self.qsize * 16) as usize;
            let slot = usize::from(self.avail_count % 128);
            ram[avail + 4 + slot * 2..avail + 6 + slot * 2].copy_from_slice(&0u16.to_le_bytes());
            self.avail_count += 1;
            ram[avail + 2..avail + 4].copy_from_slice(&self.avail_count.to_le_bytes());
            blk.vio.write(vreg::QUEUE_NOTIFY, 0);
        }

        /// usedのidxと最後のエントリのlen
        fn used(&self, ram: &[u8]) -> (u16, u32) {
            let end = self.base + self.qsize * 16 + 6 + self.qsize * 2;
            let used = ((end + 4095) & !4095) as usize;
            let idx = u16::from_le_bytes([ram[used + 2], ram[used + 3]]);
            let slot = usize::from((idx.wrapping_sub(1)) % 128);
            let len = u32::from_le_bytes(
                ram[used + 4 + slot * 8 + 4..used + 8 + slot * 8 + 4]
                    .try_into()
                    .unwrap(),
            );
            (idx, len)
        }
    }

    /// 読み: ディスクのセクタ2の中身がゲストRAMに現れ、statusはOK、
    /// usedのlenは「装置が書いた量」(データ512+status1)
    #[test]
    fn a_read_request_copies_the_sector_into_guest_ram() {
        let mut image = vec![0u8; 4096];
        image[1024..1536]
            .iter_mut()
            .enumerate()
            .for_each(|(i, b)| *b = i as u8);
        let mut blk = VirtioBlk::new(image);
        let mut ram = vec![0u8; 1 << 20];
        let mut drv = FakeDriver::new(&mut blk, 16); // リングは64KB地点

        drv.submit(&mut blk, &mut ram, req::IN, 2, 0x8000, 512, true);
        let mut writes = Vec::new();
        assert!(blk.vio.take_notify(), "呼び鈴が鳴っている");
        let irq = blk.process(&mut ram, |a, n| writes.push((a, n)));

        assert!(irq, "済んだら割り込み");
        assert_eq!(ram[0x8000], 0, "セクタ2の先頭");
        assert_eq!(ram[0x8000 + 255], 255, "中身が順に並ぶ");
        assert_eq!(ram[0x7F0], status::OK, "statusはOK");
        assert_eq!(drv.used(&ram), (1, 513), "used: 1件済み、書いた量=512+1");
        assert!(writes.contains(&(0x8000, 512)), "DMAの申告がある");
    }

    /// 書き: ゲストRAMの中身がディスクに入り、読み返すと同じ
    #[test]
    fn a_write_request_lands_on_the_disk_image() {
        let mut blk = VirtioBlk::new(vec![0u8; 4096]);
        let mut ram = vec![0u8; 1 << 20];
        let mut drv = FakeDriver::new(&mut blk, 16);
        ram[0x8000..0x8200].fill(0xAB);

        drv.submit(&mut blk, &mut ram, req::OUT, 3, 0x8000, 512, false);
        let _ = blk.process(&mut ram, |_, _| {});

        assert_eq!(blk.image[1536], 0xAB, "セクタ3に書けた");
        assert_eq!(blk.image[2047], 0xAB);
        assert_eq!(blk.image[1535], 0, "手前のセクタは触らない");
        assert_eq!(ram[0x7F0], status::OK);
    }

    /// ディスクの外を指す要求はIOERR。**パニックしない**のが装置の礼儀
    /// (表を書くのはゲストで、ゲストのバグでホストは死なない)
    #[test]
    fn out_of_range_requests_answer_ioerr() {
        let mut blk = VirtioBlk::new(vec![0u8; 4096]);
        let mut ram = vec![0u8; 1 << 20];
        let mut drv = FakeDriver::new(&mut blk, 16);

        drv.submit(&mut blk, &mut ram, req::IN, 100, 0x8000, 512, true); // セクタ100は無い
        let _ = blk.process(&mut ram, |_, _| {});
        assert_eq!(ram[0x7F0], status::IOERR);

        drv.submit(&mut blk, &mut ram, req::GET_ID, 0, 0x8000, 20, true);
        let _ = blk.process(&mut ram, |_, _| {});
        assert_eq!(&ram[0x8000..0x800C], b"rustx86-disk", "名札は読める");
    }

    /// 容量は設定空間で名乗る。ゲストはこの8バイトで /dev/vda の大きさを知る
    #[test]
    fn capacity_shows_up_in_device_config() {
        let mut blk = VirtioBlk::new(vec![0u8; 9 * 512]);
        let cap: Vec<u8> = (0..8).map(|i| blk.vio.read(vreg::CONFIG + i)).collect();
        assert_eq!(u64::from_le_bytes(cap.try_into().unwrap()), 9, "9セクタ");
    }

    /// 1AF4:1001の名乗り — Linuxのvirtio_pciはこの値でカードを選ぶ
    #[test]
    fn identity_is_virtio_block() {
        let f = pci_function(IRQ_BLK);
        let c = f.config();
        assert_eq!(
            u16::from_le_bytes([c[reg::VENDOR_ID], c[reg::VENDOR_ID + 1]]),
            0x1AF4
        );
        assert_eq!(
            u16::from_le_bytes([c[reg::DEVICE_ID], c[reg::DEVICE_ID + 1]]),
            0x1001
        );
        assert_eq!(c[reg::REVISION], 0, "legacyはリビジョン0の約束");
        assert_eq!(
            u16::from_le_bytes([c[reg::SUBSYS_ID], c[reg::SUBSYS_ID + 1]]),
            0x0002,
            "サブシステムIDが本当の型番 (2=ブロック)"
        );
        assert_eq!(f.bar_base(0), BLK_IO_BASE);
    }
}
