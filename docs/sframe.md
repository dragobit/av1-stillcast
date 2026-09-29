# SWITCH_FRAME (SFRAME) — 仕様意味と stillcast への適用余地

> 前書き: [av1-vs-vp9.md](av1-vs-vp9.md) §6 で「SFRAME による非キーフレーム復帰点」を将来の機会として挙げた。本文書はその仕様上の正確な意味 — 「キーフレームではないイントラフレーム」なのか、シーク点になるのか、stillcast で使えるか — を構文レベルで確定する。出典は AV1 spec §5.9(uncompressed header)と glossary。コード側は `src/frame_header.rs` / `src/uheader.rs` / `src/inspect.rs` / `src/assemble.rs` が既に SFRAME を正しくパースしている(生成側の話)。

## 1. 仕様が言う SFRAME

spec glossary の定義(和訳):

> 「シーケンス間を切り替える地点として使える**インターフレーム**。全参照フレームを上書きするが、イントラ符号化は強制しない。意図された使い道: 動画を短いチャンク(例: 1 秒)に分け各チャンクを SFRAME で始め、帯域が落ちたらサーバが低ビットレート版のチャンク列に切り替える。その際 SFRAME のインター予測は、既にデコード済みの高品質側参照フレームを使ってデコードされる。フルキーフレームのコストなしにビットレート切替ができる」

つまり SFRAME は「キーフレームのような状態リセットを行う**インターフレーム**」であり、「キーフレームではないイントラフレーム」ではない — 後者は `INTRA_ONLY_FRAME` の説明で、別物。

`frame_type == SWITCH_FRAME`(値 3)を選ぶと、uncompressed header で以下が**強制**される(フィールド自体がワイヤ上に出ない):

| 強制値 | 効果 |
|---|---|
| `error_resilient_mode = 1` | CDF 学習状態・MV 場など前フレーム由来の状態に依存しない → `primary_ref_frame = PRIMARY_REF_NONE` が自動成立 |
| `refresh_frame_flags = allFrames` | **8 つ全ての参照スロットを自分で上書き** → デコード後の DPB が完全に決定的 |
| `frame_size_override_flag = 1` | フレームサイズを必ず明示 = 切替先が別解像度でもよい(ABR 想定) |

一方で KF との重要な違い:

- `KEY_FRAME && show_frame` は `RefValid[i]=0`, `RefOrderHint[i]=0`, `OrderHints[]=0` をリセットするが、**SFRAME はこれをしない** — order_hint の数列が切れずに連続する。
- `showable_frame = (frame_type != KEY_FRAME)` より、表示される SFRAME は **showable**。KF は `show_existing_frame` の再表示ターゲットに**なれない**が、SFRAME は**なれる**。これが stillcast にとっての本質的な差。

## 2. 4 つの frame_type の比較

| | KEY_FRAME | INTRA_ONLY_FRAME | INTER_FRAME | SWITCH_FRAME |
|---|---|---|---|---|
| 構文上の分類 (`FrameIsIntra`) | intra | intra | inter | **inter** |
| イントラ符号化強制 | する | する | しない | しない(任意) |
| `error_resilient_mode` | 強制 1(表示 KF) | 信号化 | 信号化 | **強制 1** |
| `refresh_frame_flags` | 強制全(表示 KF) | 信号化 | 信号化 | **強制全(0xFF)** |
| order_hint/RefValid リセット | する(表示 KF) | しない | しない | しない |
| `show_existing` で再表示可能 | **不可** | 可 | 可 | **可** |
| ストリーム先頭になれる | 可 | 可 | 不可 | **不可**(下記) |

## 3. 「シーク点にできる」の精度 — 仕様 vs 実装

- **冷起動(白紙)でのデコードは適合規則上できない。** インターフレームの `ref_frame_idx[i]` は「`RefValid[ref_frame_idx[i]] == 1`」の適合要件を持ち、ストリーム先頭では全スロットが invalid。SFRAME が切替点であり得るのは、**DPB が既に何かで埋まっている前提**があるから(中身が別ビットレート版でもよい)。7 参照は全て同一スロットを指してよい(同一性の要件なし)。
- **実装上は途中開始が効くことが多い。** 全ブロックをイントラで書けば参照画素は実際には不要なので、dav1d のようなデコーダは寛容に解く(spec 側にも途中開始デコーダ向けの扱いがある)。つまり「仕様的には switch point、実装寛容性で疑似シーク点」。
- **プレイヤーが実際にシークするかはコンテナ層の話。** mp4 では stss(sync sample)に載るものだけがシーク先になり、muxer/parser は一般に `KEY_FRAME` しか sync と見なさない。IVF でもパーサの KF フラグに依存。**規格上の切替点 ≠ プレイヤーのシーク点** — 「互換性を気にする」直感が当たるのはこの層。

## 4. stillcast への適用余地

現行の per-画像構造は `anchor(表示 KF) + golden(表示・再表示可・スロット更新)` の **2 実符号化フレーム**(input-contract.md)。KF は再表示できないため golden が要る。

SFRAME 導入案:

```
ストリーム先頭 KF(1 回だけ)
  + 各画像 i:  SFRAME_i(表示・showable・全スロット上書き) + SE×(gop-1)
```

- SFRAME は「DPB リセット」と「再表示ターゲット」を 1 枚で兼ねる → **画像あたり実符号化フレームが 2→1**。stillcast の価値(符号化フレーム数の最小化)に直結する。
- 全ブロックをイントラで書けば、各セグメント先頭は前セグメントの画素内容に依存しない(DPB は「何か valid があればよい」だけで、画素は使わない)。
- さらに、SFRAME はインター予測が合法なので「前画像と共有する要素(背景・枠・字幕バー)を前セグメントの参照から部分予測」して縮められる可能性 — ただしその場合は前セグメントの DPB 画素に依存するチェーンになる。
- order_hint がリセットされない点も stillcast の order_hint 管理と相性が良い(KF だと数列が 0 に戻る)。

**生成経路の現実**: この環境の ffmpeg 4.4 + libaom-av1 ラッパには SFRAME 化オプションが無い。既知の経路は (a) libaom API の `AOM_EFLAG_IS_SFRAME` フレームフラグ、(b) SVT-AV1 の `--sframe-dist` / `--sframe-mode` パラメータ(OTT チャンネル切替向け実装)。(c) stillcast の uheader 手術で作る場合、INTER→SFRAME は ER(1bit)+refresh(8bit)+size_override の除去等のビットパック再構成になり、INTRA_ONLY→SFRAME は逆に参照系フィールド群の挿入になる — いずれも「タイルデータが全イントラブロック」のときのみ意味を持つ。エンコーダネイティブ生成が筋としては素直。

## 5. 採用前に必要な検証

1. SFRAME 実ストリームの生成(SVT-AV1 または libaom API 経由)→ dav1d でのデコード + `show_existing_frame` 再表示の動作確認(この環境の ffmpeg 4.4 には libsvtav1 が無いため別途導入要)
2. ffmpeg av1 parser / mp4 muxer が SFRAME を sync sample/シーク点として扱うか(プレイヤー層の実態)
3. HW デコーダ・ブラウザでの `frame_type=3` 耐性(野外での実績が薄い構文)
4. サイズ比較: 現行 `KF + golden` vs `KF + SFRAME 単体`(同一画質・同一画像)

## 6. 問いへの直接回答

- **「キーフレームではないイントラフレーム?」→ それは `INTRA_ONLY_FRAME`。** SFRAME は構文上インターフレーム(ただし全ブロックをイントラで書くことは合法)。「全参照を自分で上書きする + ER 強制 + 再表示可能」が本質。
- **「シーク点にできるのは初耳? 互換性で見送った?」→ 採用・見送りの記録はなく、[av1-vs-vp9.md](av1-vs-vp9.md) で初めて候補として表面化した。** 見送られたわけではない。ただし互換性の懸念は正しい場所を突いている: 規格上の切替点になれても、プレイヤーのシークテーブルやコンテナの sync マーキングは KF 前提で作られているため、実効的なシーク点化には検証が要る。
- **「stillcast で発揮することはある?」→ あり得る。** セグメント先頭が「リセット+再表示ターゲット」を兼務すれば画像あたりの実符号化フレームを半減できる。生成手段(エンコーダ側出力 or ヘッダ手術)とデコーダ/プレイヤー互換の実測が次の課題。

---

*出典: AV1 Bitstream & Decoding Process Specification §5.9 / glossary; [input-contract.md](input-contract.md)(現行 anchor/golden 契約)、[av1-vs-vp9.md](av1-vs-vp9.md) §6(機会としての初出)。*
