//! Unicorn Engine (QEMUのCPUライブラリ化) をオラクルとした比較実行。
//! ランダム命令列を自作CPUとUnicornの両方で実行し、レジスタとフラグを突き合わせる。
//! `cargo test -p rustx86-cosim` で実行 (デフォルトビルド対象外)。
