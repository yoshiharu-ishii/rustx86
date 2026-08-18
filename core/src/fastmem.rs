//! fastmem 段2 (ADR-0026): ゲスト**線形**空間のホストミラー。
//!
//! 考え方: ソフトTLBが「変換の写し」を持つように、ここは**写像そのもの**を
//! 持つ — 線形ページ la>>12 がホストの `mirror_base + la` で直接読めるよう、
//! RAM実体 (GuestRamの共有バッキング) を線形配置どおりにもう一度mmapする。
//!
//! ## v1は「フォルトさせない」設計
//!
//! Dolphin式のSIGSEGV+バックパッチは踏まない。代わりに**有効表** (1バイト/
//! ページ、bit0=カーネル可読・bit1=ユーザ可読) を引いてから読む — Rustや
//! 生成コードの真ん中でシグナルから復帰する地雷を避け、外れは従来ヘルパへ
//! 落ちるだけ。シグナル絞り込みは効果を実測してから (台帳)。
//!
//! ## 正しさの契約 (ソフトTLBと同じ意味論)
//!
//! - 張るのは **translate_missが権限検査を通した変換だけ** (Aビットも
//!   その歩きが立てる — ミラー読みはTLBヒット読みと同じ「二度目以降」)
//! - 剥がすのはソフトTLBと同じ合図 (invlpg / mov cr3 / cr0変更 /
//!   スナップショット復元)。**guestがinvlpgせずPTEを書き換えたら嘘をつく**
//!   のは実TLB・ソフトTLBと同じ非アーキテクチャ領域
//! - v1は**読みだけ** (書きミラーはコードページ/VRAM/Dirtyの契約が絡む —
//!   段3)。読みには副作用が無いので、内容はRAM実体の別名でつねに最新
//!
//! ## 16KiBホストページ (Apple Silicon)
//!
//! mmapは16KiB粒度なので、線形16KiB群 (4KiBゲストページ×4) の**全員が
//! 物理でも連続・16KiB整列**のときだけ張れる。兄弟3ページの変換は
//! **ソフトTLBを覗いて**手に入れる (歩き直すとAビットが立って挙動が変わる
//! — 覗くだけなら無害)。Linuxホスト (4KiBページ) は1ページずつ張れる。

use std::cell::{Cell, RefCell};

/// 線形4GiB / 4KiB = 2^20 ページの有効表 (1MiB)。0=無効
pub const OK_PAGES: usize = 1 << 20;
pub const OK_KERNEL_R: u8 = 1;
pub const OK_USER_R: u8 = 2;

pub struct Fastmem {
    /// 線形ミラーの先頭 (4GiB予約)。`mirror + la` で読む
    mirror: *mut u8,
    /// 有効表の先頭 (Box領有)。生成コードが直接引くので番地は固定
    ok: Box<[Cell<u8>; OK_PAGES]>,
    /// RAM実体のfd (GuestRamと共有 — dupして自前で持つ)
    fd: libc::c_int,
    ram_len: usize,
    /// ホストページ/ゲストページ比 (1 or 4)。張る単位 = この数の連続ページ
    group: usize,
    /// 張った群の先頭ページ番号 (剥がし用、G/非Gで別居 —
    /// mov cr3の非Gフラッシュが**Gの山を歩かない**ため)。順序不問
    mapped_g: RefCell<Vec<u32>>,
    mapped_ng: RefCell<Vec<u32>>,
    /// 観測: 累積の張り/全剥がし回数
    fills: Cell<u64>,
    flushes: Cell<u64>,
}

const RESERVE: usize = 4usize << 30;

impl Fastmem {
    /// GuestRamの共有バッキングfdからミラーを構える。失敗はNone (fastmem無効)
    pub fn new(ram_fd: libc::c_int, ram_len: usize) -> Option<Fastmem> {
        unsafe {
            let host_page = libc::sysconf(libc::_SC_PAGESIZE) as usize;
            if host_page < 4096 || !host_page.is_multiple_of(4096) {
                return None;
            }
            let fd = libc::dup(ram_fd);
            if fd < 0 {
                return None;
            }
            let mirror = libc::mmap(
                std::ptr::null_mut(),
                RESERVE,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            );
            if mirror == libc::MAP_FAILED {
                libc::close(fd);
                return None;
            }
            Some(Fastmem {
                mirror: mirror as *mut u8,
                ok: Box::new([const { Cell::new(0) }; OK_PAGES]),
                fd,
                ram_len,
                group: host_page / 4096,
                mapped_g: RefCell::new(Vec::new()),
                mapped_ng: RefCell::new(Vec::new()),
                fills: Cell::new(0),
                flushes: Cell::new(0),
            })
        }
    }

    /// 生成コード用: ミラー先頭とok表先頭 (Machineのフィールドへ写す)
    pub fn mirror_base(&self) -> usize {
        self.mirror as usize
    }
    pub fn ok_base(&self) -> usize {
        self.ok.as_ptr() as usize
    }

    /// translate_missが通した変換を差し込む。`user_ok` はPTE由来 (U/S)。
    /// 群 (16KiBホストページなら4ページ) が揃わなければ何もしない —
    /// 兄弟の変換は呼び手がソフトTLBから覗いて渡す
    ///
    /// # 群の判定
    /// `lin_page` は群整列済みの線形ページ番号 (呼び手が保証)。
    /// siblings[i] = 群内iページ目の (物理ページ番号, user_ok, global)。
    /// 全員が物理連続・群整列で、RAM内に収まるときだけ張る。
    /// **全員G**のときだけ群をグローバル扱い (mov cr3を生き延びる)
    pub fn note_fill(&self, lin_page: u32, siblings: &[(u32, bool, bool)]) {
        debug_assert_eq!(siblings.len(), self.group);
        // 物理: 群整列 + 連続
        let base_pfn = siblings[0].0;
        if base_pfn as usize % self.group != 0 {
            return;
        }
        for (i, &(pfn, _, _)) in siblings.iter().enumerate() {
            if pfn != base_pfn + i as u32 {
                return;
            }
        }
        let global = siblings.iter().all(|&(_, _, g)| g);
        // (張り先の記録は末尾で — mmap成功後)
        let phys = (base_pfn as usize) << 12;
        let len = self.group << 12;
        if phys + len > self.ram_len {
            return; // RAM外 (MMIO/空洞) は張らない — 従来経路が裁く
        }
        debug_assert_eq!(lin_page as usize % self.group, 0);
        let lin = (lin_page as usize) << 12;
        unsafe {
            let p = libc::mmap(
                self.mirror.add(lin) as *mut libc::c_void,
                len,
                libc::PROT_READ,
                libc::MAP_SHARED | libc::MAP_FIXED,
                self.fd,
                phys as libc::off_t,
            );
            if p == libc::MAP_FAILED {
                return;
            }
        }
        for (i, &(_, user_ok, _)) in siblings.iter().enumerate() {
            let ok = OK_KERNEL_R | if user_ok { OK_USER_R } else { 0 };
            self.ok[lin_page as usize + i].set(ok);
        }
        if global {
            self.mapped_g.borrow_mut().push(lin_page);
        } else {
            self.mapped_ng.borrow_mut().push(lin_page);
        }
        self.fills.set(self.fills.get() + 1);
    }

    /// 全部剥がす (mov cr3 / cr0変更 / 復元)。予約ごとPROT_NONEを一発で
    /// 被せ、有効表は張った群だけ消す (1MiBのmemsetを毎回はしない)
    pub fn flush_all(&self) {
        self.flushes.set(self.flushes.get() + 1);
        let mut mapped_g = self.mapped_g.borrow_mut();
        let mut mapped = self.mapped_ng.borrow_mut();
        if mapped.is_empty() && mapped_g.is_empty() {
            return;
        }
        unsafe {
            let p = libc::mmap(
                self.mirror as *mut libc::c_void,
                RESERVE,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,
                -1,
                0,
            );
            debug_assert!(p != libc::MAP_FAILED);
        }
        for lin_page in mapped.drain(..).chain(mapped_g.drain(..)) {
            for i in 0..self.group {
                self.ok[lin_page as usize + i].set(0);
            }
        }
    }

    /// 非グローバルだけ剥がす (mov cr3 — PGEの正規の意味論)。
    /// G群は写像も有効表も生かす: カーネル半分がプロセス切替を生き延びる
    pub fn flush_nonglobal(&self) {
        self.flushes.set(self.flushes.get() + 1);
        let len = self.group << 12;
        for lin_page in self.mapped_ng.borrow_mut().drain(..) {
            unsafe {
                let p = libc::mmap(
                    self.mirror.add((lin_page as usize) << 12) as *mut libc::c_void,
                    len,
                    libc::PROT_NONE,
                    libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,
                    -1,
                    0,
                );
                debug_assert!(p != libc::MAP_FAILED);
            }
            for i in 0..self.group {
                self.ok[lin_page as usize + i].set(0);
            }
        }
    }

    /// 1ページ剥がす (invlpg)。群ごと落とす (mmapの粒度が群なので)
    pub fn flush_page(&self, la: u32) {
        let group_head = ((la >> 12) as usize / self.group * self.group) as u32;
        if self.ok[group_head as usize].get() == 0 {
            return;
        }
        unsafe {
            let p = libc::mmap(
                self.mirror.add((group_head as usize) << 12) as *mut libc::c_void,
                self.group << 12,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,
                -1,
                0,
            );
            debug_assert!(p != libc::MAP_FAILED);
        }
        for i in 0..self.group {
            self.ok[group_head as usize + i].set(0);
        }
        self.mapped_g.borrow_mut().retain(|&g| g != group_head);
        self.mapped_ng.borrow_mut().retain(|&g| g != group_head);
    }

    /// 観測: (今張っている群, うちG群, 累積で張った群, フラッシュ回数)
    pub fn stats(&self) -> (usize, usize, u64, u64) {
        let g = self.mapped_g.borrow().len();
        let ng = self.mapped_ng.borrow().len();
        (g + ng, g, self.fills.get(), self.flushes.get())
    }

    /// 観測: 張っている群の数
    pub fn mapped_groups(&self) -> usize {
        self.mapped_g.borrow().len() + self.mapped_ng.borrow().len()
    }

    /// ホストページ/ゲストページ比 (張る単位)
    pub fn group(&self) -> usize {
        self.group
    }

    /// この群はもう張ってあるか (先頭ページで判定)
    pub fn is_mapped(&self, lin_page: u32) -> bool {
        self.ok[lin_page as usize].get() != 0
    }
}

impl Drop for Fastmem {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.mirror as *mut libc::c_void, RESERVE);
            libc::close(self.fd);
        }
    }
}

unsafe impl Send for Fastmem {}
unsafe impl Sync for Fastmem {}
