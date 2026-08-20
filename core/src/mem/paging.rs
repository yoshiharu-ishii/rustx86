//! ページング — 線形→物理の変換 (2段の表歩き)・TLB・#PFの記録。
//!
//! 熱いのは [`Machine::translate_for`] のTLBヒット直線路だけで、
//! ミス側 (表歩き+フィル) と初回書き込み (Dビット) は #[cold] の別関数。

use crate::{Machine, PageFault, TlbEntry, TLB_INVALID, TLB_SLOTS};

/// ページウォーク1回の結果。物理先頭・権限に加えて、
/// A/Dビットの書き戻し先 (表の側の物理番地) も運ぶ
struct Walk {
    base: u32,
    writable: bool,
    user_ok: bool,
    /// PDEの物理番地 (Aビットの宛先)
    pde_addr: u32,
    /// 葉 (PTE、4MBページならPDE) の物理番地 (A/Dビットの宛先)
    leaf_addr: u32,
}

impl Machine {
    /// TLBを全部空にする。mov cr3 (アドレス空間の切り替え) や CR0 の変更、
    /// スナップショット復元の後に呼ぶ。**表を書き換えたのに写しが古いと
    /// 幽霊のページが見え続ける**ので、切り替えの合図で必ず捨てる
    pub fn tlb_flush(&self) {
        for slot in &self.tlb {
            let mut e = slot.get();
            e.tag = TLB_INVALID;
            slot.set(e);
        }
    }

    /// TLBの1ページだけ無効化する (INVLPG)。ページテーブルの1エントリを
    /// 書き換えたカーネルは、この命令でそのページの写しだけを捨てる
    pub fn tlb_flush_page(&self, la: u32) {
        let slot = ((la >> 12) as usize) & (TLB_SLOTS - 1);
        let mut e = self.tlb[slot].get();
        e.tag = TLB_INVALID;
        self.tlb[slot].set(e);
    }

    /// 線形アドレスを物理アドレスへ。
    ///
    /// **ここがページングの正体**である。CR0.PGが立っていなければ線形=物理。
    /// 立っていれば、上位20bitで2段の表を引く:
    ///   線形 [31:22]=ディレクトリ番号 [21:12]=テーブル番号 [11:0]=ページ内オフセット
    ///
    /// TLB (変換の写し) はまだ持たない。決定的なので**毎回歩いても結果は同じ**で、
    /// 速度が問題になるまで足さない (「測ってから足す」— docs/reference/ci.md と同じ流儀)。
    ///
    /// こちらは**寛容な版** (デバッガ・ツール用)。未マップは RAM 外の番地を
    /// 返し、読めば 0xFF になる。CPUの実行経路は [`translate_for`](Self::translate_for) を使い、
    /// 失敗を #PF として配送する
    pub fn translate(&self, la: u32) -> u32 {
        if self.cpu.cr0 & 0x8000_0000 == 0 {
            return la;
        }
        // 覗き見はゲストを変えない: TLBにも控えず、A/Dビットも立てない。
        // 権限も見ない (デバッガはU/Sに縛られず覗けるほうが正しい)
        self.walk_page(la)
            .map(|w| w.base | (la & 0xFFF))
            .unwrap_or(0xFFFF_FFFF)
    }

    /// CPUのアクセス経路の変換。**ページ保護もここで裁く**:
    ///   - present が無ければ不在フォールト
    ///   - 書き込みで R/W=0 のページは、CR0.WP (リング0でも守る) か
    ///     リング3なら保護フォールト。カーネルはこの挙動を起動時に試験し、
    ///     #PFが来ないと「壊れたWP」として起動を拒否する (実際に拒否された)
    ///
    /// TLBヒットの直線路だけを持つ (C11: hot/cold分離)。ミス (表歩き+フィル+
    /// Aビット) と、書き込みの初回 (Dビット) は #[cold] の別関数へ —
    /// 全メモリアクセスがここを通るので、インライン展開される機械語を
    /// ヒット路の大きさに保つ
    #[inline]
    pub fn translate_for(&self, la: u32, write: bool) -> Result<u32, PageFault> {
        if self.cpu.cr0 & 0x8000_0000 == 0 {
            return Ok(la); // PG off: 線形がそのまま物理
        }
        // --- TLBを引く。当たれば表を歩かない ---
        if cfg!(feature = "opstats") {
            self.tlb_probes.set(self.tlb_probes.get() + 1);
        }
        let vpn = la >> 12;
        let slot = (vpn as usize) & (TLB_SLOTS - 1);
        let e = self.tlb[slot].get();
        if e.tag != vpn {
            if cfg!(feature = "opstats") {
                self.tlb_misses.set(self.tlb_misses.get() + 1);
            }
            return self.translate_miss(la, write, slot);
        }
        let base = e.base_flags & !0xFFF;
        // --- 権限チェック。CPLとWPは引くたびに新しく (sys_accessも) ---
        let user = self.cpu.cpl() == 3 && !self.sys_access.get();
        let wp = self.cpu.cr0 & 0x0001_0000 != 0;
        if write && e.base_flags & 1 == 0 && (user || wp) {
            return Err(PageFault {
                la,
                write,
                present: true,
            });
        }
        if user && e.base_flags & 2 == 0 {
            return Err(PageFault {
                la,
                write,
                present: true,
            });
        }
        // dirty=「Dを立てた後か」の控え — 立てた後は書き込みでも表に触らない
        if write && e.base_flags & 4 == 0 {
            return self.translate_set_dirty(la, slot, e);
        }
        Ok(base | (la & 0xFFF))
    }

    /// TLBミス側: 表を歩いて権限を裁き、通った変換だけを控えて
    /// Aビット (と初回書き込みならDビット) を帳簿に積む。
    /// A/D は実CPUがページ表へ書き戻す2ビット (Linuxはこれで「最近触った/
    /// 汚れた」を知り、test386のPOST 11はこれを検査する)。
    /// 歩く経路は &self なので直接は書けず、queue_ad → 命令境界のflush_ad
    #[cold]
    #[inline(never)]
    fn translate_miss(&self, la: u32, write: bool, slot: usize) -> Result<u32, PageFault> {
        // 不在フォールトのW/Rビットは**このアクセスの向き** — walkは向きを
        // 知らないので、エラーコードの材料はここで書き足す
        let w = self.walk_page(la).map_err(|mut f| {
            f.write = write;
            f
        })?;
        // 権限チェック (ヒット路と同じ判定を、歩いた値で)
        let user = self.cpu.cpl() == 3 && !self.sys_access.get();
        let wp = self.cpu.cr0 & 0x0001_0000 != 0;
        if write && !w.writable && (user || wp) {
            return Err(PageFault {
                la,
                write,
                present: true,
            });
        }
        if user && !w.user_ok {
            return Err(PageFault {
                la,
                write,
                present: true,
            });
        }
        // 検査を通った変換だけが控えと帳簿を書く
        self.tlb[slot].set(TlbEntry {
            tag: la >> 12,
            base_flags: w.base
                | w.writable as u32
                | ((w.user_ok as u32) << 1)
                | ((write as u32) << 2),
            leaf: w.leaf_addr,
        });
        self.queue_ad(w.pde_addr, 0x20);
        if w.leaf_addr != w.pde_addr {
            self.queue_ad(w.leaf_addr, 0x20);
        }
        if write {
            self.queue_ad(w.leaf_addr, 0x40);
        }
        Ok(w.base | (la & 0xFFF))
    }

    /// ヒットしたが D 未設定のページへの書き込み初回: 控えに dirty を立て、
    /// 表へのDビット書き戻しを帳簿に積む (ページごとに一度きりの道)
    #[cold]
    #[inline(never)]
    fn translate_set_dirty(&self, la: u32, slot: usize, e: TlbEntry) -> Result<u32, PageFault> {
        let base = e.base_flags & !0xFFF;
        self.tlb[slot].set(TlbEntry {
            tag: la >> 12,
            base_flags: e.base_flags | 4,
            leaf: e.leaf,
        });
        self.queue_ad(e.leaf, 0x40);
        Ok(base | (la & 0xFFF))
    }

    /// 2段の表を歩いて、ページの物理先頭と権限ビット、そして**表の側の番地**
    /// (Aビットを立てるPDE、Dビットを立てる葉) を返す (TLBミス時のみ)。
    /// **不在は Err(present:false)** — これは TLB に載せない (次回また歩く)
    fn walk_page(&self, la: u32) -> Result<Walk, PageFault> {
        let notp = || PageFault {
            la,
            write: false,
            present: false,
        };
        let dir = (la >> 22) & 0x3FF;
        let pde_addr = (self.cpu.cr3 & !0xFFF) + dir * 4;
        let pde = self.read_phys32(pde_addr);
        if pde & 1 == 0 {
            return Err(notp());
        }
        if pde & 0x80 != 0 {
            // 4MBページ (PSE): テーブルを引かず、ディレクトリで直に物理が決まる。
            // TLBは4K単位なので、この4Kぶんの物理先頭を作る。A/DともPDEに立つ
            let base = (pde & 0xFFC0_0000) | (la & 0x003F_F000);
            return Ok(Walk {
                base,
                writable: pde & 2 != 0,
                user_ok: pde & 4 != 0,
                pde_addr,
                leaf_addr: pde_addr,
            });
        }
        let tbl = (la >> 12) & 0x3FF;
        let pte_addr = (pde & !0xFFF) + tbl * 4;
        let pte = self.read_phys32(pte_addr);
        if pte & 1 == 0 {
            return Err(notp());
        }
        // R/W・U/S は2段の**厳しい方**が効く (両方立って初めて許す)
        Ok(Walk {
            base: pte & !0xFFF,
            writable: pde & 2 != 0 && pte & 2 != 0,
            user_ok: pde & 4 != 0 && pte & 4 != 0,
            pde_addr,
            leaf_addr: pte_addr,
        })
    }

    /// 変換失敗を記録する (最初の1件だけ)。命令の終わりで #PF になる
    pub(crate) fn note_fault(&self, f: PageFault) {
        if self.pending_fault.get().is_none() {
            self.pending_fault.set(Some(f));
        }
    }

    /// REPの一括処理用: 線形アドレス `la` から、**同じページ内で連続して
    /// 触れる物理範囲**を返す。`write` は書き込みか。
    /// 返り値は (物理先頭, そのページで残るバイト数)。フォールトなら None
    /// (呼び出し側が note_fault 済みのつもりで巻き戻す)。
    /// RAMを超える範囲は None (遅い道に落とす)
    pub(crate) fn phys_span(&self, la: u32, write: bool) -> Option<(usize, usize)> {
        let pa = match self.translate_for(la, write) {
            Ok(pa) => pa,
            Err(f) => {
                self.note_fault(f);
                return None;
            }
        };
        let page_remain = 0x1000 - (la & 0xFFF) as usize;
        let a = pa as usize;
        if a + page_remain > self.mem.len() {
            return None;
        }
        Some((a, page_remain))
    }
}
