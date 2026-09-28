# 他コーデックへの適用性分析: stillcast の手法は AV1 以外にどこまで持ち出せるか

stillcast は「静的画像の長時間動画」を、数枚の実符号化フレームと、残りをすべて `show_existing_frame` (「保存済み参照フレームをもう一度表示せよ」) 命令だけの TU で構成するビットストリームアセンブラである。本書はまず stillcast が AV1 で**何を**やっているのかをビットレベルまで分解し、続いてその各機構が他の映像コーデックで再現可能かを個別に検証する。検証は仕様書の読みだけでなく、可能なものは実際にビット列を構築して ffmpeg/libvpx でデコード確認した実測値を併記する (実測環境: ffmpeg 4.4.2, libx264/libx265/libvpx/libxvid 系, 2026-09)。

## エグゼクティブサマリ

| コーデック | ゼロコスト再表示プリミティブ | 反復フレーム実測/推定コスト | stillcast 適用性 |
|---|---|---|---|
| AV1 | `show_existing_frame` f(3) | **~6 B/TU** (現行) | 基準系 |
| **VP9** | `show_existing_frame` f(3) — AV1 の直接の祖先 | **~1 B/frame (実測・デコード検証済)** | ほぼ直移植。むしろ AV1 より簡単な部分がある (golden 不要化・seq header 不在) |
| **AV2 (AVM)** | `OBU_REGULAR_SEF`/`OBU_LEADING_SEF` として存続 | 数 B/TU (AV1 同様) | 仕様 v1.0.0 でプリミティブ存続確認。エンコーダ未到達が唯一の壁 |
| **MPEG-4 Part 2 (ASP)** | `vop_coded=0` の N-VOP — spec 上は存在 | ~6 B/VOP (構築可能) だが **ffmpeg デコーダが出力フレームを出さない** (実測) | 仕様適合は成立、実用はデコーダ互換リスク大 |
| **H.264/AVC** | なし | 手作り all-skip P スライス **~11 B/frame (実測・デコード検証済)**; x264 自然出力 22–40 B | ゼロコスト再表示は不可能。全スキップスライス合成で十分実用的 |
| **HEVC** | なし (逆方向 `pic_output_flag` のみ) | x265 自然 ~74 B; all-skip CTU で~20–60 B 見通し | H.264 と同じ全スキップ経路が可能 |
| **VVC** | なし (`ph_pic_output_flag` のみ) | 未測定 (エンコーダ未整備) | 構造上可能だが実務上の優先度低 |
| **VP8** | なし (`show_frame=0` = 逆方向のみ) | libvpx 自然 ~183 B; all-skip で縮小可 | 全スキップ経路のみ |
| **MPEG-2** | `repeat_first_field` (フィールド反復、最大1.5倍まで) | mpeg2video 自然 ~666 B; all-skip P で削減可 | 放送 TS で歴史ある実用経路あり (静止画チャンネル) |
| H.263 | なし | all-skip P-MB | 同上・マイナー |
| JPEG 系/MJPEG/ProRes/DNxHD/FFV1/Snow/VC-2 | なし (全フレーム独立) | フルフレーム | 不可 (動画ビットストリームとしては) |
| GIF/APNG/animated WebP | — | — | **別パラダイム**: 表示時間をメタデータに持つ (後述) |
| コンテナ層 (mp4 stts 等) | — | **0 B** (サンプル自体を増やさない) | 第3の経路: ビットストリームに触れずサンプル表示時間を伸ばす (後述) |

結論だけ先に言えば: **stillcast の手法の心臓部 (show_existing_frame) は VP9 にそのまま存在し、しかも AV1 より制約が緩い**。H.264/HEVC はプリミティブこそ無いが、stillcast の*建築* (ビットストリームアセンブラ + GOP ポリシー + コストモデル) は全スキップスライス合成としてほぼそのまま移植でき、手作りスライスで ~11 B/frame まで実測で落とせた。MPEG-4 ASP の N-VOP は仕様上存在するが実装側の扱いが不統一。イントラオンリー系は原理的に不可能。そして「ビットストリームを増やさずコンテナの表示時間だけ伸ばす」第3の道が常にある — stillcast がそれを選ばなかった理由も明確に説明できる。

---

## 第1部: stillcast が AV1 で実際にやっていること — 超高解像度分解

移植性を論じる前に、stillcast の「AV1 でやっていること」を、単なる「show_existing_frame を連発する」よりも細かく、機械的な作業単位に分解する。分解の結果、stillcast は 1 つのプリミティブ依存の中心機構と、8 つのほぼコーデック中立な周辺機構からできている、というのが本書の主張の土台になる。

### 1.1 層モデルの再掲

`docs/transport-io.md` が定めた4層モデル:

```
1. syntax unit      OBU (ヘッダ + ペイロード)
2. grouping         TU = 1 decode/display ステップの OBU 列
3. serialization    low-overhead OBU stream | Annex-B
4. container        IVF / MP4 / MKV (時刻・音声・メタデータ)
```

stillcast の変換は**層2 (TU) で完結**している。これが移植議論の地図になる: プリミティブの有無は層1–2 (シンタクスと参照モデル) の話、コンテナ互換は層4の話、と分けて考えられる。

### 1.2 機構分解: 各パーツが AV1 の何に依存しているか

以下、stillcast を構成する 9 つの機構を個別に分解し、それぞれの AV1 依存度を採点する (◎=コーデック中立の一般設計 / ○=AV1 特有だが他コーデックに明確な対応物あり / ▲=AV1 固有で他コーデックでは別プリミティブか断念 / ×=他コーデックに存在しない)。

#### (a) 実フレームの取得 — anchor + golden の入力契約 ◎

`split_input` がやっていること: 先頭から TU をスキャンし (上限 `INPUT_SCAN_LIMIT=256` は病的入力ガードのみ)、**anchor TU** (sequence header + shown KEY_FRAME を含むもの) と **golden TU** (anchor 直後に符号化された shown・非キー・showable・1つ以上の参照スロットを更新するフレーム) のペアを条件ベースで見つける。位置インデックスではなく意味条件で判定するため、先頭のフレームレス TU、重複 sequence header、invisible frame、エンコーダの内部フレーム落とし (WebCodecs realtime モードのサイレントドロップ等) をすべて許容する。

- **AV1 固有の点**: 「golden = showable で refresh する非キーフレーム」という条件自体 (後述の showable_frame 制約の反映)。
- **中立な点**: 「実エンコーダの最小出力をスキャンして契約を満たすフレームを拾う」という設計パターン自体は完全にコーデック中立。H.264 版なら「SPS/PPS + IDR + 最初の非 IDR P スライス」、MPEG-4 版なら「VOL + I-VOP」が対応物になる。`decode-adjacent` 条件 (golden が anchor 直後に符号化されていること、間に参照スロットを更新する coded frame を挟まないこと) も参照モデルに依存する一般的な概念。

#### (b) 参照フレームの永続化と DPB 管理 ○

AV1 の DPB は 8 スロットの明示的な参照バッファモデルで、各 coded frame が `refresh_frame_flags` (8 bit) でどのスロットを更新するかを宣言する。stillcast の golden は「表示される・非キー・showable・refresh_frame_flags≠0」を満たす最初のフレームで、アセンブラはその `refresh_frame_flags` の最下位ビット位置を `golden_slot` とし、以後の show_existing TU はすべてそのスロットを指す。

- **依存点**: 8 スロットの明示参照モデルと refresh flags 構造は AV1/VP9 系の設計。VP9 は同じく 8 バッファ (`refresh_frame_flags` 同様) で直移植可能。H.264/HEVC 系は「参照ピクチャリスト + MMCO/DPB 管理」の違うモデル — スロットを指して「これを表示しろ」という命令自体が存在しない (ただし全スキップ経路では参照リストの先頭=直前フレームで十分なので、実用上は等価物が要らない)。
- **興味深い差異**: AV1 では shown KEY_FRAME が全 8 スロットを自身で初期化するため、GOP 先頭の keyframe の後に来る golden TU は GOP ごとにバイト同一で良い (リセット後の DPB は毎回同じ状態)。これは stillcast の「golden TU を clone するだけ」設計を可能にしている AV1 特有の性質。VP9 も keyframe が全参照バッファを初期化するので同じ性質を持つ。

#### (c) show_existing_frame TU の合成 ▲ (本命プリミティブ)

`assemble.rs::show_existing_tu` が生成する TU のビット列を分解すると:

```
TU = TD OBU + FRAME_HEADER OBU
  TD OBU      : 0x12 0x00            (2 B — OBU type 2, empty payload)
  FRAME_HEADER: 0x1A <size=1> 0x??   (3 B ヘッダ + 1 B ペイロード)
  ペイロード 1 B = show_existing_frame f(1)=1 | frame_to_show_map_idx f(3) | trailing_bits
```

合計 **~6 B/TU**。これがストリームの ~97% を占める。この TU は「新しいピクチャをデコードせず、DPB スロット `frame_to_show_map_idx` の内容を出力せよ」とだけ言う。タイルデータは一切続かない — `OBU_FRAME` ではなく `OBU_FRAME_HEADER` (type 3) でなければならないという spec 制約を満たすため、ヘッダだけの OBU を送っている。

- **依存点**: このプリミティブ自体が AV1/VP9 系 (と後述の AV2) にのみ存在する。H.264/HEVC/VVC/MPEG 系には一切存在しない。これが移植可否の最大の分岐点。

#### (d) GOP 構造 — shown KEY_FRAME による周期的リセット ◎

stillcast の GOP 構造: `keyframe TU → golden TU → show_existing TU ×(gop-2)` の繰り返し。shown KEY_FRAME はデコーダ状態を完全リセットし、全 8 参照スロットを自身で初期化するため、**どの GOP でも同じ key TU・golden TU・show_existing TU のバイト列が使い回せる** (上記 (b))。keyframe 間隔が seek 粒度を決める。

- **中立な点**: GOP = 「リセット点 + 反復表示」の繰り返しという構造はどのコーデックにも写る。H.264 なら IDR + P-skip、MPEG-4 なら I-VOP + (N-VOP or skip P-VOP)、MPEG-2 なら I-picture + skip-P-picture。
- **AV1 特有の利点**: AV1 の KEY_FRAME は「intra 符号化」と「デコーダリセット」が同じ概念に結合している。H.264 は IDR (=リセット) と non-IDR I (=intra のみ) を分ける構造を持つ — 実際には seek アンカーは IDR にするのが安全なので実用上の差は小さいが、設計の表現力としては AV1/VP9 の「keyframe = 常にリセット」のほうが素直。

#### (e) sequence header の正規化と注入 ○

`seq_header.rs` は sequence header を全フィールド保存で解析し、必要に応じて以下を行う:

- `timing_info` 未宣言の入力には constant-rate timing_info を注入 (出力 fps と `equal_picture_interval` を一致させる)
- `--decoder-model` 時は `decoder_model_info` を追記し、実フレームの frame header に `buffer_removal_time_present_flag=0` をビット単位で splice (`uheader.rs` の uncompressed-header 走査で挿入位置を計算)
- playlist では全 segment が byte-identical sequence header を持つことを強制

- **依存点**: sequence header という独立したストリーム構造は AV1/VP9/AV2 系の設計。H.264/HEVC 系では SPS/PPS/VPS が同等物で、「全 segment の SPS/PPS バイト一致」が同じ制約になる。timing/HRD メタデータの注入は H.264 では VUI/HRD パラメータに対応するが、既存 SPS のビット列に部分書き換えを行う箇所が対応 — 設計は同じ、対象シンタクスが違う。
- **重要な差異**: **VP9 には sequence header が存在しない** (全メタデータが keyframe の uncompressed header に入る)。playlist の「byte-identical seq header」制約は、VP9 では「全 segment の keyframe uncompressed header が同じ geometry/profile/color 設定」を持つことに写る。

#### (f) order hint と表示順モデル ○

AV1 の `order_hint` は表示順のヒントで、stillcast は「golden の order_hint が anchor+1 であること」を decode-adjacent 検証に使う (hint がないストリームではスキップして decode-order adjacency のみに依存)。show_existing では保存済みフレームの `RefOrderHint` がそのまま使われ、spec が明示的に「OrderHint は真の表示順を反映しなくてよい」と認めているため合法。

- **中立な点**: 表示順メタデータは H.264 の POC (pic_order_cnt)、HEVC の PicOrderCntVal、MPEG-4 の time_increment/modulo_time_base に相当する一般概念。どれも「各 access unit が表示時刻を持つ」モデル。

#### (g) playlist — 複数静止画の時間切替 ◎

`--playlist`: 各画像は独立した (key TU, golden TU) ペア = segment として符号化され、segment 境界は必ず shown KEY_FRAME。条件は全 segment で byte-identical sequence header (寸法・色・エンコーダ設定を揃える)。

- **中立な点**: 「セグメント境界 = キーフレーム = リセット点 + seek point」の設計は全コーデック共通。H.264 版なら全 segment で同じ SPS/PPS、VP9 版なら同じ keyframe ヘッダ、MPEG-4 版なら同じ VOL。

#### (h) コストモデル — size↔seek frontier ◎

`stillcast plan` のコストモデルは閉形式:

```
video_bytes ≈ n_gops × (kf_size + golden_size) + (total_frames − 2·n_gops) × se_size
seek_error < gop フレーム
```

`kf_size`/`golden_size`/`se_size` は probe encode で**実測**する (予測ではなく)。`--target-seek` は gop = N×fps、`--max-size` は gop を幾何級数的に増やすか CRF ラダーで keyframe を小さくする。

- **中立な点**: このモデルの形はどのコーデックでも同じ。変わるのは `se_size` (AV1: ~6 B, VP9: ~1 B, H.264 all-skip: ~11–30 B, HEVC all-skip: ~20–74 B, MPEG-4 N-VOP: ~6 B but 互換リスク) と `kf_size` (コーデックの符号化効率による)。`plan` のアルゴリズム自体は一切変わらない。

#### (i) コンテナ/トランスポート層 ○

- **入力**: IVF (`DKIF`) / low-overhead OBU / Annex-B を content sniff で自動判別、正規化は `IvfFile` = `(ts, TU)` 列に統一。
- **出力**: IVF ライタと MP4 ライタ (`av01` sample entry + `av1C` + `stss` sync table + faststart 2-pass)。

- **依存点**: IVF の fourcc はコーデック識別子を持つ (`AV01`, `VP90`, `VP80`) が、IVF の構造自体は VP8/VP9/AV1 で共有されている — **IVF 層は文字通りそのまま使い回せる** (fourcc を変えるだけ)。MP4 側はコーデック登録情報が違うだけで構造は同じ: `av01`+`av1C` → `vp09`+`vpcC` / `avc1`+`avcC` / `hvc1`+`hvcC` / `mp4v`+`esds`。`stss` (sync sample table) は全コーデック共通。

#### (j) 検証・診断 ◎

`stillcast info`/`--check` は「最初の TU が shown KEY_FRAME」「show_existing が keyframe を指さない」「各 TU が 1 フレームを表示」「非キー coded frame は INTER+showable」等の不変条件を構造的に検証する。e2e は libdav1d でピクセル一致を確認。

- **中立な点**: 構造検証 + 参照デコーダでのピクセル一致という検証パターンはコーデック中立。各コーデックでは不変条件の具体形が変わる (例: H.264 では「各 access unit が 1 枚を出力」「IDR が seek 点」「全 P スライスが all-skip」等)。

#### (k) 決定性・非圧縮・速度 ◎

stillcast の変換は圧縮を一切行わない純粋な bitstream→bitstream 変換で、1 時間 @30fps の展開が ~30 ms、同じ入力には byte-identical 出力。

- **中立な点**: この性質は「実フレームを既成エンコーダに任せ、アセンブラは構造だけを制御する」という層分離設計の必然的帰結であり、どのコーデックに移植しても成立する。

### 1.3 分解のまとめ: 本命は (c) だけ — 残りは設計として移植可能

| 機構 | AV1 依存度 | 移植時の作業 |
|---|---|---|
| (a) 入力契約スキャン | ◎ 中立 | 各コーデックの anchor/golden 条件を再定義するだけ |
| (b) DPB/参照管理 | ○ AV1/VP9 型 | 全スキップ経路では不要 (参照=直前ピクチャで十分) |
| (c) show_existing TU 合成 | ▲ **本命** | プリミティブの有無が全てを決める |
| (d) GOP 構造 | ◎ 中立 | IDR/I-VOP で同じ |
| (e) seq header 正規化 | ○ 同等物あり | SPS/PPS/VPS、VP9 では keyframe header |
| (f) order hint/POC | ○ 同等物あり | 各コーデックの表示順モデル |
| (g) playlist | ◎ 中立 | segment=キーフレーム境界は共通 |
| (h) コストモデル | ◎ 中立 | 定数を probe encode で取り直すだけ |
| (i) コンテナ層 | ○ 構造共通 | sample entry 名と config record が変わるだけ |
| (j) 検証 | ◎ 中立 | 不変条件の具体形を書き直す |
| (k) 決定性 | ◎ 中立 | 設計が自動的に保証 |

つまり stillcast の移植性は、事実上「**(c) のプリミティブがそのコーデックに存在するか、存在しない場合に何で代替するか**」に集約される。以下はその分析。

---

## 第2部: ゼロコスト再表示プリミティブの有無で分類

### 2.1 「デコードせずに表示する」命令の系譜

`show_existing_frame` は AV1 が発明したものではない。**VP9 が持っていた機構を AV1 が `showable_frame` 制約付きで継承した**もの。さらに系譜を遡ると、MPEG-4 Part 2 (ASP) の `vop_coded=0` (いわゆる N-VOP) が「ピクチャを新たに符号化せず、表示だけ進める」同等の意味論を持つ。そして今後の AV2 (AVM) でも show_existing 機構が専用 OBU タイプ (`OBU_REGULAR_SEF`/`OBU_LEADING_SEF`) として存続している (AV2 Bitstream & Decoding Process Specification v1.0.0, av2.aomedia.org で確認)。

一方 H.264/HEVC/VVC にはこの種のプリミティブは存在しない。これらのコーデックでは「デコードされたピクチャがそのまま 1 回だけ出力される」のが規則で、表示だけを独立に命令する構文がない。HEVC/VVC にある `pic_output_flag`/`ph_pic_output_flag` は逆方向 (デコードするが表示しない) であり、これは「非表示の参照フレームを仕込む」には使えるが「既存フレームを再表示する」には使えない。

### 2.2 プリミティブの等価表

| プリミティブ | コーデック | 意味論 | コスト |
|---|---|---|---|
| `show_existing_frame` + `frame_to_show_map_idx` | AV1 | DPB スロット指定で再表示 (showable 制約あり) | ~6 B/TU (TD+OBU header+1 B payload) |
| 同上 | VP9 | 参照バッファ指定で再表示 (**showable 制約なし**) | **~1 B/frame** (uncompressed header のみ) |
| `OBU_REGULAR_SEF`/`LEADING_SEF` | AV2 | 同上 (構造は刷新) | 数 B |
| `vop_coded=0` (N-VOP) | MPEG-4 ASP | 直前の表示 VOP を再表示 | ~6 B/VOP |
| `repeat_first_field` | MPEG-2 | フィールド反復 (最大 1.5× の表示延長) | 0 B — ただし bounded, interlace 前提 |
| なし → all-skip スライス | H.264/HEVC/VVC/VP8/H.263 | 残差ゼロ・MVゼロの P/B スライスで「前フレームをそのまま再符号化」 | 11–100+ B/frame |

---

## 第3部: コーデック別詳細評価

### 3.1 VP9 — ほぼ直移植、むしろ一部単純化 ★本命

#### プリミティブの存在とビットレベル仕様

VP9 uncompressed header (spec §6.2):

```
uncompressed_header() {
  frame_marker          f(2) = 0b10
  profile_low_bit       f(1)
  profile_high_bit      f(1)
  [if profile==3: profile_reserved f(1)]
  show_existing_frame   f(1)
  if (show_existing_frame) {
    frame_to_show_map_idx  f(3)
    return   // ここで終了 — 後続データなし
  }
  frame_type            f(1)
  ...
}
```

つまり show_existing frame とは**ヘッダ 1 バイトだけ**のフレーム: profile 0 で slot 0 を指すなら `0x88` 1 バイト (`10 0 0 1 000` + trailing)。AV1 よりさらに軽い — AV1 は OBU ラッパ (TD + FRAME_HEADER OBU) で ~6 B だが、VP9 の raw frame は packet としてそのまま 1 B で済む。

#### 実測検証 (本分析で実施)

libvpx-vp9 で作った通常エンコード (keyframe 1 枚 + 手作り show_existing パケット 29 個) を IVF に包み、ffmpeg ネイティブ VP9 デコーダと libvpx-vp9 の両方でデコード検証:

- **30 フレーム全てデコード・出力された** (packet 30 個 → frame 30 個)
- **全フレームがピクセル一致** (show_existing が keyframe の内容を正しく再表示)
- ストリーム合計: keyframe 28483 B + 29×1 B = **28904 B / 30 フレーム** (反復部分 ~0.26 kbps @30fps)

#### stillcast 移植時に VP9 で簡単になる点

1. **golden フレームが要らない**: AV1 には `showable_frame` 制約 (shown keyframe は show_existing で再表示できない) があるため、表示用に非キーの golden が必須だった。VP9 にはその制約が**ない** — 参照バッファにある任意のフレーム (keyframe を含む) を再表示できる。つまり **入力契約が「keyframe 1 枚だけ」に単純化される**: anchor + golden の 2 フレーム契約が 1 フレーム契約に縮退する。
2. **sequence header が要らない**: VP9 には sequence header OBU がなく、メタデータは keyframe の uncompressed header に内包される。playlist の「全 segment の seq header byte-identical」制約は「全 segment の keyframe が同じ geometry/profile/color config」に写る — 検査はむしろ容易。
3. **IVF はそのまま使える**: fourcc を `VP90` にするだけ。

#### 注意点・差異

- **error_resilient_mode**: VP9 のエラーレジリエントモードは参照バッファ更新を抑制する。libvpx のデフォルトで show_existing が使えるかは実装依存 — 今回は libvpx-vp9 で問題なく動いたが、ターゲットデコーダでの確認は必要。
- **superframe index**: VP9 パケットは末尾に superframe index を持つことがある (複数フレームを 1 パケットにまとめる構造)。IVF ではパケット=フレームなので不要だが、他のコンテナでは処理が必要。
- **互換性**: VP9 hw decode は AV1 より広い古い機器に行き渡っている (2015 年〜の Android/Chrome/Smart TV 等)。ブラウザは Chrome/Firefox/Edge は OK、Safari は最近の Apple Silicon + VP9 hw のみ — AV1 より広いが万能ではない。

#### 判定

stillcast-vp9 は最も実現性の高い派生。~1 B/frame は AV1 の ~6 B/TU よりさらに小さく、golden 不要化で入力契約も単純化される。唯一の実質的課題は「libvpx/ffmpeg 経由で任意の画像から keyframe を生成し、これを show_existing で再表示する」までのパイプライン — これは stillcast の orchestration 層をほぼそのまま流用できる。

### 3.2 AV2 (AVM) — プリミティブ存続、エンコーダ未到達

AV2 の Bitstream & Decoding Process Specification v1.0.0 (av2.aomedia.org 公開) では、show-existing 機能が `OBU_REGULAR_SEF` / `OBU_LEADING_SEF` という専用 OBU タイプとして存続している。AVM (AV2 参照ソフトウェア) のコードベースは AV1 を継承しており、show_existing_frame の機構も保持されている (CWG-F356 "Clarification for the show existing frame obu" 等のドラフト履歴が確認できる)。

- **実現性**: 仕様上は AV1 と同じアーキテクチャがそのまま使える。stillcast のコアロジックは AV2 の OBU 構造 (刷新されたヘッダフィールド名) に合わせて移植するだけ。
- **障壁**: 一般利用可能な AV2 エンコーダが存在しない (AVM はリサーチコードであり実用エンコーダではない)。デコーダも市場にほぼない。実用化は 2027 年以降の見通し。

### 3.3 MPEG-4 Part 2 (ASP) — spec 上は存在するが実装が不統一

MPEG-4 Part 2 (DivX/Xvid 系) には、AV1 show_existing_frame の先駆けとも言える **N-VOP** (`vop_coded=0`) がある。VOP ヘッダの `vop_coded` ビットが 0 のとき、その VOP は「符号化されていない」=「直前の表示 VOP をそのまま再表示せよ」を意味し、VOP データ本体は省略される。

#### N-VOP の構造 (実際に構築したもの)

```
00 00 01 B6          VOP start code
vop_coding_type f(2) = 01 (P)
modulo_time_base f(1+) = 0  (0 個の '1' + terminator '0')
marker_bit f(1) = 1
time_increment f(5)  (VOL の time_increment_resolution=30 → 5 bits)
marker_bit f(1) = 1
vop_coded f(1) = 0   ← NOT CODED
```

合計 ~6 B/VOP (start code 4 B + ヘッダ 2 B)。

#### 実測結果と問題

構築したストリーム ([VOL+I-VOP] + 29 N-VOP) を ffmpeg の mpeg4 デコーダに通した結果:

- **デコーダは N-VOP パケットを消費したが、出力フレームを一切生成しなかった** — 30 packets → decoded 1 frame (I-VOP のみ)
- 同じストリームで N-VOP の後に coded P-VOP を続けても N-VOP 分の出力は現れず

つまり ffmpeg の mpeg4 デコーダは N-VOP を「表示イベント」としては扱わず、パケットをスキップして次に進む。これは AVI 時代の N-VOP 再生 (各チャンクが表示タイムスタンプを持ち、プレイヤーが前フレームを表示し続ける) とは異なる処理で、現代の libavcodec ベースのデコードパイプラインでは N-VOP からの表示フレーム生成は実装されていない。

- libxvidcore (Xvid 独自デコーダ) では N-VOP が「直前 VOP の再表示」として出力を生成する可能性は残る (Xvid デコーダ自体は `vop_coded=0` を正しく処理する設計) が、本環境では libxvid デコーダが利用できず未検証。
- **評価**: spec 上は完全に合法だが、実際のデコーダ挙動がプラットフォームで不統一。stillcast-mpeg4 は「実現可能だが配布先のデコーダを選ぶ」という位置づけ。AVI エコシステムでの配布なら実用性は高い。

#### 全スキップ経路の参考値

- ffmpeg ネイティブ mpeg4 エンコーダの静止画反復: ~2040 B/frame (N-VOP を出さず毎回 coded P-VOP)
- libxvid 同条件: ~387 B/frame (coded P-VOP、MB スキップ主体)

### 3.4 H.264/AVC — プリミティブなし、だが全スキップ経路が十分実用的

H.264 には「デコード済みピクチャの再表示」という構文はない。デコードされた各 access unit は必ず 1 枚の表示ピクチャを生成し、表示だけを別途命令する手段がない。以下、検討した代替プリミティブ:

- **MMCO** (memory_management_control_operation): 参照フレームのマーキング制御のみ。表示には無関係。
- **Redundant pictures** (`redundant_pic_cnt`): 同一ピクチャの冗長符号化 — 表示反復ではなく、実ビットを要する。
- **SEI pic_timing / ct_type**: テレシネ用メタデータでフィールド反復を示唆するが、表示動作を保証しないヒントであり信頼できない。
- **POC 衝突**: 複数 access unit に同じ POC を与えるのは非合法。

→ **全スキップ P スライス** (または全スキップ B スライス) が唯一の経路: 新しい access unit は符号化するが、その中身が「全部スキップ」= 前ピクチャのピクセルをそのままコピー + 残差ゼロ。

#### 実測と手作り検証

x264 自然出力 (静止画入力, `-bf 0`):

| モード | 反復フレーム中央値 | 最小 | 最大 |
|---|---|---|---|
| CABAC (default) | 29 B | 22 B | 1011 B |
| CAVLC (`-coder 0`) | 118 B | 27 B | 1326 B |

x264 の CABAC パスが既に ~29 B まで来ている。しかし真の下限はもっと低い — **手作りで全スキップ CAVLC P スライスを構成したところ 11 B/frame でデコード成功** (ffmpeg ネイティブデコーダで全フレームがピクセル一致で出力されることを確認)。

手作りスライスの構成 (640×640, POC type 2, CAVLC):

```
NAL header (0x41) + slice header:
  first_mb_in_slice ue=0 | slice_type ue=0 (P) | pps_id ue=0 | frame_num f(4)
  num_ref_idx_active_override f(1)=1 | num_ref_idx_l0_active ue=0 (1 ref)
  ref_pic_list_mod f(1)=0 | luma_log2_weight ue=0 | chroma ue=0
  luma_weight_l0[0] f(1)=0 | chroma_weight_l0[0] f(1)=0
  adaptive_ref_pic_marking f(1)=0 | slice_qp_delta se=0
  disable_deblocking_filter_idc ue=1
slice data: mb_skip_run ue(1600)   ← 40×40 MB 全スキップを 1 つの Exp-Golomb 符号で
```

合計 ~11 B/フレーム (start code 4 B + NAL 1 B + RBSP ~6 B)。

- **frame_num はフレームごとにインクリメントが必要** (H.264 の規則: 参照フレームの frame_num は参照ストアド管理に使われる)。AV1 的な「同じ TU を clone」はできず、**フレーム番号だけを書き換えるテンプレート生成**になる — 依然として完全に決定的で高速だが、バイト同一 clone ではない点は AV1 との実装差。
- **CABAC は不向き**: CABAC の P-skip は MB ごとにコンテキスト符号化されたフラグを書くため ~1–2 B/MB (1600 MB で ~200–400 B) に膨らむ。対して CAVLC は `mb_skip_run` というラン長符号が 1 つの Exp-Golomb (ue(1600) = 21 bit ≈ 3 B) で済む。**静的映像では CAVLC が CABAC を逆転する** — 一般的には CABAC が優れる例外として面白い。
- **IDR**: seek アンカーは IDR access unit (=AV1 の shown KEY_FRAME に相当)。IDR 以降は DPB がクリアされるので、全スキップ経路でも same-as-AV1 の GOP 構造が取れる。

#### MP4/コンテナ

`avc1` sample entry + `avcC` (SPS/PPS リスト) + `stss`。H.264 の mp4 登録は最も成熟しており、対応性は全コーデック中最強。BSF/パーサ/プレイヤーの互換性リスクがほぼない。

#### 判定

stillcast-h264 は「プリミティブはないが、全スキップテンプレート合成で ~11 B/frame の再表示が実現できる」+「最大のエコシステム互換性」で、VP9 系と並ぶ実用性を持つ。AV1 版の ~6 B/TU にはわずかに劣るが、x264 自然出力 (~29 B) との差を考えれば手作りスライス合成による ~2.6 倍の削減は意味がある。

### 3.5 HEVC (H.265) — H.264 と同じ構造、実効コストはやや高め

HEVC も「デコード済みピクチャを再表示」という構文は持たない。HEVC にあるのは `pic_output_flag` (slice header 内、PPS の `output_flag_present_flag` が立っているとき) — これは「**このピクチャをデコードするが表示しない**」= 非表示 golden のような逆方向の機構であり、stillcast 的な「表示だけする」には使えない。

- **全スキップ CTU**: HEVC の P/B スライスで全 CTU を skip (sao_merge/skip フラグ) にすれば同じことができる。CTU サイズ 64×64 で 640×640 なら ~100 CTU、CABAC コンテキスト符号化される skip フラグが CTU ごとに入るので ~20–80 B/frame が見通し。
- **実測**: x265 静止画反復で ~74 B/frame (median)。
- **DPB**: HEVC の参照管理はより複雑だが、all-skip では「直前ピクチャからの予測」で十分。
- **コンテナ**: `hvc1`/`hev1` + `hvcC`。対応性は H.264 より狭い (Apple/モダン機器は OK、古い機器は不可)。

### 3.6 VVC (H.266) — 構造上可能だが実務優先度低

VVC にも `ph_pic_output_flag` (ピクチャヘッダ内、decode-not-output) はあるが、再表示プリミティブはない。all-skip CTU は可能だが、VVC の実用デコーダ/エンコーダ整備がまだ浅い (VVenC/VVdeC 等リファレンス実装はあるがエコシステムが狭い)。現時点では stillcast 移植の実務意義は小さい。

### 3.7 VP8 — 逆方向のみ、全スキップ経路で代替

VP8 には `show_existing_frame` が**ない** (VP9 で追加された)。VP8 の uncompressed header にあるのは `show_frame` フラグ (フレームをデコードするが表示しない) — これは AV1 の `show_frame=0`/`showable_frame` と同じ逆方向機構であり、表示反復は不可能。

- 実測: libvpx (VP8) 静止画反復 ~183 B/frame。
- **全スキップ P フレーム**が唯一の経路で、VP8 でも skip MB は probability-coded されるが、構造上 ~10–20 B/frame まで絞れる見通し (未検証)。
- エコシステム: VP8 は WebM/旧 Android 向けだが今日では AV1/VP9 がカバーするので、stillcast-vp8 の実用意義はほぼない。

### 3.8 MPEG-2 — repeat_first_field は制限付きプリミティブ、TS 配信では歴史ある実用

MPEG-2 はインターレース前提の設計で、`picture_coding_extension` の `repeat_first_field` フィールドが「先頭フィールドをもう一度表示する」というプリミティブを持つ:

- プログレッシブフレーム + `repeat_first_field=1` + `top_field_first` パターンで、そのフレームの表示時間を 2 フィールドから 3 フィールドに延長 (= 1.5×) できる。これは映画の 3:2 プルダウン (24fps→60i) で日常的に使われてきた機構。
- ただしこれは **bounded** な延長 (1.5 倍まで) であって、AV1 のような無限反復ではない。それでも放送 TS では、静止画チャンネル (音声のみ + ロゴ) で「I-picture + repeat_first_field の連鎖」や「低頻度 I-picture 更新」が実際に使われてきた歴史がある。
- **all-skip P-picture**: MPEG-2 の P ピクチャで全マクロブロックを skip にすれば同じアプローチが取れる。実測 mpeg2video エンコーダの静止画反復は ~666 B/frame — 手作りすれば大幅に削れる。
- **MPEG-TS の特殊性**: TS には mp4 の stts のような「サンプル毎の表示時間」テーブルがなく、表示時間は PCR/PTS で駆動される。つまり **TS ではコンテナ層トリックが使えない** — ビットストリームに実 access unit を入れるしかない。ここで MPEG-2 の repeat_first_field や all-skip P-picture のようなビットストリーム内機構が真の意味を持つ。

### 3.9 H.263 — 全スキップ P-MB、実用性は歴史的経緯のみ

H.263 (3GPP 時代のビデオ電話) にも show-existing 相当はない。P ピクチャの全スキップマクロブロックで同様のことができるが、2026 年現在では実用デプロイ意義はほぼない。構造分析としてのみ記す。

### 3.10 イントラオンリー / 静止画列 — 原理的に不可能

- **Motion JPEG**: 各フレームが独立した JPEG — 参照・再表示の概念がない。
- **ProRes / DNxHD / VC-2 / FFV1 / Snow / Dirac (VC-2)**: すべてイントラまたはフレーム独立性が高く、再表示プリミティブもスキップ経路もない。
- **PNG/JPEG 静止画コンテナ**: 動画ではない。

これらに stillcast 手法を「適用」することは原理的に不可能 — ただし逆に、これらのフレームを AV1/VP9 にエンコードして stillcast で展開する方向は成り立つ (実際、stillcast の `make` はそうしている)。

### 3.11 GIF / APNG / animated WebP — 別パラダイム

これらは「表示時間を各フレームのメタデータとして持つ」フォーマットであり、「フレーム 1 枚 + duration 1 時間」という記述が可能 — stillcast 的な「フレームを反復して出す」必要がそもそもない。

- **しかし**: これらは**動画ビットストリームではない**。seek table、A/V 同期 (音声トラックが持てない)、フレームレートベースの再生モデルがない。静止画配信としては完璧だが、「音声付き動画としての seek/sync」という stillcast の解決対象を満たさない。用途が違う代替品として整理するのが正しい。

---

## 第4部: 第3の道 — コンテナ層での表示時間伸長

### 4.1 mp4 stts による「サンプル増殖なし」アプローチ

mp4/ISOBMFF では各サンプルの表示時間は `stts` (Time to Sample) box の delta で決まる。つまり**サンプルを 1 つだけ置き、その delta を巨大にすれば、ビットストリームを一切増やさずに「同じピクチャを長時間表示」が実現できる**。実際に検証した: 1 枚の H.264 フレームを持つ mp4 の stts delta を 30 s 相当に書き換えたところ、ffprobe が duration=30.0 を正しく報告し、デコーダは 1 フレームをデコードした。

同じことが Matroska (BlockDuration)、WebM にも言える。さらに mp4 の `elst` (edit list) でエントリを繰り返すことも可能。

### 4.2 これが stillcast の選択でない理由

stillcast がこの道を選ばない (あるいは選べない) のは:

1. **実フレームレートのストリームが欲しいケース**: プレイヤーやブラウザの `<video>` は「N フレーム @ fps」という構造を期待することが多く、1 サンプル=30 s は「実質 0.03 fps の動画」として扱われる。ブラウザのデコーダパイプライン、サムネイル生成、進行バーの表示が正しく機能するかは保証されない。
2. **シーク**: mp4 のシークは `stss`/`stts`/`stsc`/`stsz`/`stco` テーブルを辿って目的時刻のサンプルに着陸する。1 サンプルしかないストリームではどこにシークしても同じサンプルに戻る — 意味論としては正しいが、実装によっては変な挙動をするリスクがある。
3. **放送/TS では不可能**: MPEG-TS に stts 相当はない。放送・ライブ配信ではビットストリーム内の実フレームが必須。
4. **A/V 同期の保持**: 音声と独立に進む映像で、プレイヤーが「長いフレーム」をどう扱うかは実装依存。stillcast の bitstream-level 反復は「各フレームが本当に存在する」ので、どんな実装でも確実に表示される。

つまり、stillcast は「ビットストリームに反復を書き込む」ことで、**どんなデコーダ/プレイヤー/コンテナでも正しく動く最大公分母**を選んでいる。これは設計として意図的な選択であり、代替経路としての stts 伸長を理解した上での判断と読める。

### 4.3 比較

| 経路 | ビットストリーム増殖 | デコーダ負荷 | コンテナ依存 | 実用度 |
|---|---|---|---|---|
| show_existing (AV1/VP9) | TU を生成 (数 B/フレーム) | 表示のみ | なし | ◎ |
| N-VOP (MPEG-4) | VOP を生成 (数 B) | 表示のみ | なし | ▲ 実装依存 |
| all-skip slice (H.264/HEVC) | スライスを生成 (10–100 B) | 軽いデコード | なし | ○ |
| repeat_first_field (MPEG-2) | 0 (ヘッダビット) | 0 | なし (TS でも動く) | ▲ bounded 1.5× |
| stts/BlockDuration 伸長 | **0** (ビットストリームに触れない) | 0 | **mp4/MKV 限定** | ▲ 実装依存 |
| フレーム duration メタ (GIF/APNG) | 0 | 0 | なし (動画ではない) | — 別用途 |

---

## 第5部: 実測データ集約

全て同一入力 (640×640, 30fps, 120 フレーム静止画, jacket.png) での ffmpeg エンコード測定:

| codec | エンコーダ | 1st frame | 反復中央値 | 反復 min/max | 総量 | 備考 |
|---|---|---|---|---|---|---|
| AV1 | libaom-av1 crf36 cpu8 | 17893 B | 21 B | 21–38 B | 20474 B | libaom の自然 skip inter |
| VP9 | libvpx-vp9 crf36 | 28483 B | 26 B | 26–31 B | 31582 B | |
| VP8 | libvpx crf32 | 42358 B | 183 B | 60–2690 B | 75881 B | |
| H.264 | x264 default (CABAC) | 24978 B | 29 B | 22–920 B | 29850 B | |
| H.264 | x264 CABAC -bf 0 | 23860 B | 40 B | 27–1011 B | 30036 B | |
| H.264 | x264 CAVLC -bf 0 | 27721 B | 118 B | 27–1326 B | 41321 B | x264 では CAVLC が重い |
| HEVC | x265 crf30 | 19750 B | 74 B | 57–140 B | 28483 B | |
| MPEG-4 | native mpeg4 | 32308 B | 2040 B | 1984–32308 B | 547860 B | N-VOP を出さない |
| MPEG-4 | libxvid | 31920 B | 387 B | 335–31920 B | 374080 B | coded P-VOP |
| MPEG-2 | mpeg2video | 31353 B | 666 B | 599–31353 B | 475400 B | |
| (crafted) VP9 SE | — | 28483 B | **1 B** | 1 B | ~28900 B | 手作り+2 デコーダで検証済 |
| (crafted) MPEG-4 N-VOP | — | 32301 B | **6 B** | 6 B | ~32500 B | 構築したが ffmpeg 出力 0 |
| (crafted) H.264 all-skip | — | ~27700 B | **11 B** | 11 B | ~28000 B | 手作り+ffmpeg で検証済 |
| (container) mp4 stts | — | — | **0 B** | — | ~27600 B | 1 サンプル、30 s duration |

エンコーダ自然出力との比較が stillcast 系ツールの意義を示す: 例えば H.264 でも x264 自然 (29 B) vs 手作り全スキップ (11 B) で ~2.6 倍差、VP9 では libvpx 自然 (26 B) vs show_existing (1 B) で 26 倍差。

---

## 第6部: 移植ロードマップ案 (優先度順)

### Tier 1 — すぐ価値が出る

1. **stillcast-vp9**: show_existing_frame がそのまま存在し、しかも AV1 より制約が緩い (golden 不要)。実装は stillcast のコアをほぼ流用、シリアライゼーション層を IVF (fourcc VP90) と MP4 (vp09/vpcC) に合わせるだけ。対象: AV1 デコードを持たない古い HW/Smart TV/旧 Android。
2. **stillcast-h264**: プリミティブはないが、全スキップ P スライステンプレートで ~11 B/frame が実測確認済み。最大のデコーダ互換エコシステム (ブラウザ/Smart TV/モバイル/組込み全て)。入力契約は「IDR + 最初の P スライス」で AV1 版より単純でもあり。

### Tier 2 — 条件付きで有効

3. **stillcast-av2**: 仕様は既に show_existing を持つが、実用エンコーダ待ち。
4. **stillcast-mpeg2-ts**: 放送 TS 向けのニッチ (音声 + 静止画チャンネル、サイネージ)。repeat_first_field + all-skip P の組み合わせで「コンテナ層を使えない場面」でも実フレーム級の反復が可能。

### Tier 3 — spec 上成立するが実装リスク

5. **stillcast-mpeg4**: N-VOP は spec 合法だが ffmpeg デコーダが出力を生成しないことが実測で判明。libxvidcore/AVI 系なら可能性はあるが、今日の配布先として優先度は低い。

### 不採用方向

- **イントラオンリー系 (MJPEG/ProRes/DNxHD/FFV1/VC-2/Snow)**: プリミティブ自体が存在せず、そもそも stillcast とは別問題。
- **VP8**: show_existing なく、VP9 が上位互換で広いため実用意義薄い。
- **H.263/EVC**: 前者は歴史的、後者はエコシステム未到達。

---

## 第7部: AV1 固有で「持ち込めないもの」「むしろ不要になるもの」の整理

### AV1 で必須だが他コーデックで不要になるもの

| AV1 制約 | VP9 | H.264/HEVC | MPEG-4 | なぜ不要か |
|---|---|---|---|---|
| golden (非キー・showable) が必須 | **不要** (keyframe も show_existing 可能) | 不要 (全スキップ経路) | 不要 | VP9 は showable_frame 制約がない |
| sequence header の byte-identical 管理 | **不要** (seq header がない) | SPS/PPS の同等管理が必要 | VOL の同等管理が必要 | VP9 はメタデータが keyframe ヘッダ内 |
| OBU_FRAME_HEADER (type 3) しか使えない | フレームそのものが 1 B | NAL type=1 スライス | VOP start code | コンテナ構造の違い |
| TD OBU (2 B/TU) | **不要** (IVF パケットだけ) | NAL 区切りは start code | VOP start code | トランスポート層の差 |
| `refresh_frame_flags` / 8スロット | 同様の 8 バッファ構造 | DPB+MMCO (モデル違う) | 直前表示のみ | 参照管理モデルの差 |
| order_hint 検証 | ない (keyframe リセットのみ) | frame_num/POC 管理 | time_increment 管理 | 表示順モデルの差 |

### AV1 で使えるが他コーデックに持ち込めないもの

- **`equal_picture_interval` + `temporal_point_info` の自由度**: AV1 の decoder model はこの設計を最大化するためにある。H.264/HEVC の HRD/VUI は別設計なので、ビット書き換えの対象が違う。
- **`reduced_still_picture_header` の拒否**: AV1 に only 存在する概念。移植先では不要な判定になる。
- **Invisible golden** (`show_frame=0` の参照フレーム): AV1/VP8/VP9/HEVC/VVC にあるが、H.264 にはない (decode=display が必然)。

---

## 第8部: 設計一般化の結論

stillcast の本質的な設計パターンを AV1 非依存の形で書き直すと:

> **「ビットストリームアセンブラ」パターン**: 実エンコーダが生成した最小限の実フレーム列を入力として受け取り、コーデック固有の「低コスト表示」プリミティブ (存在するならば再表示命令、なければ全スキップスライス合成) を使って、所望のフレームレート・GOP 構造・再生時間を持つ spec 合法ストリームを決定的に構成する。併せて、コンテナ層への写像 (sample entry + sync table + timescale) と、probe encode による実測値ベースのコストモデル、構造的不変条件の検証機構を持つ。

このパターンに対し、AV1 での唯一の本命プリミティブは show_existing_frame — 残り 8 つの機構 (入力契約スキャン、DPB 管理、GOP、seq header、order hint、playlist、コストモデル、コンテナ層、検証、決定性) は全てコーデック中立の設計である。

プリミティブの有無による 3 階層:

- **第 1 階層 — 真の再表示命令**: AV1 (show_existing), VP9 (show_existing), AV2 (SEF OBU), MPEG-4 ASP (N-VOP)
- **第 2 階層 — 全スキップ合成**: H.264, HEVC, VVC, VP8, MPEG-2, H.263 — 新しい access unit は符号化するが中身が空
- **第 3 階層 — 不可能**: MJPEG/ProRes/DNxHD/FFV1/VC-2/Snow 等のイントラオンリー

そして **第 4 の経路** として常に「コンテナの表示時間伸長」が存在する — stillcast がそれを選ばないのは「ビットストリームに実フレームがある」ことが最大の互換性を持つからであり、mp4-only 用途ならコンテナ経路のほうがさらに軽い。

---

## 付録 A: 実験手順 (再現用)

```
# 静止画ソース
ffmpeg -f lavfi -i "color=...:s=640x640:d=1" -frames:v 1 jacket.png  (gradient+noise版)

# エンコーダ自然出力の反復コスト測定
ffmpeg -loop 1 -i jacket.png -vf format=yuv420p -r 30 -frames:v 120 \
  -c:v <enc> <opts> out.<fmt>
ffprobe -show_entries packet=size out.<fmt> | median of frames[1:]

# VP9 show_existing_frame 検証
# libvpx-vp9 keyframe packet + 29x show_existing(0x88) を IVF に pack →
ffmpeg -c:v vp9  vp9_se.ivf -f null -   # native: 30 frames decoded ✓
ffmpeg -c:v libvpx-vp9 vp9_se.ivf ...   # libvpx: 30 frames decoded ✓

# MPEG-4 N-VOP 検証
# m4v VOL+I-VOP + 29x crafted N-VOP (vop_coded=0) →
ffmpeg -i m4v_nvop.m4v -f null -   # 30 packets consumed, 1 frame output
                                   # (ffmpeg decoder は N-VOP をスキップ)

# H.264 all-skip P-slice 検証
# x264 Annex-B の SPS/PPS/IDR 前方 + crafted P-slices (mb_skip_run=1600) →
ffmpeg -i h264_skip.h264 -f null -  # 15 frames decoded, all identical ✓

# mp4 単一サンプル長時間表示
# 1-frame mp4 の stts delta を 30 s に書き換え →
ffprobe duration=30.0, decode=1 frame ✓
```

## 付録 B: コーデック別の「入力契約」対応表

| コーデック | anchor 相当 | golden 相当 | 契約の最小構成 |
|---|---|---|---|
| AV1 | seq header + shown KEY_FRAME | shown non-key, showable, refreshes ≥1 slot, order_hint=anchor+1 | 2 フレーム |
| VP9 | keyframe (=all-refresh + config) | **不要** (keyframe が showable) | **1 フレーム** |
| H.264 | SPS+PPS+IDR | 不要 (全スキップ経路) または最初の P | 1–2 アクセスユニット |
| HEVC | VPS+SPS+PPS+IDR | 同上 | 同上 |
| MPEG-4 | VOL+I-VOP | 不要 (N-VOP 経路) または最初の P-VOP | 同上 |

## 付録 C: 未検証・リスク一覧

- **VP9 show_existing on hardware decoders**: ソフトウェア (ffmpeg native, libvpx) は確認済みだが、HW decoder (Android MediaCodec VP9, Smart TV) は未測定 — AV1 の >97% SE ストリームと同じく「spec 合法だが珍しい」リスク。
- **MPEG-4 N-VOP on libxvidcore**: ffmpeg では出力が生成されなかった。Xvid の独自デコーダ (ffdshow 系経路を含む) での確認が必要。
- **H.264 all-skip 手作りスライス**: ffmpeg native decoder で確認済みだが、HW decoder (Intel/ARM/Apple) とブラウザ MSE は未検証。
- **mp4 stts 伸長**: ffprobe/ffmpeg では正しく認識されるが、ブラウザ `<video>` や VLC/mpv での実再生は未検証 (再生継続・シーク動作・A/V 同期)。
- **AV2**: spec ドラフト段階、実デコーダ未存在。

以上が stillcast 手法の他コーデックへの適用性の全貌である。**本命は VP9 — show_existing_frame がそのまま存在し、制約が AV1 より緩く、1 B/frame という最小コストを実測で確認した**。次点は H.264 — プリミティブはないが全スキップスライス合成で 11 B/frame が実現可能で、互換性は全コーデック最大。MPEG-4 ASP の N-VOP は spec 上存在するがデコーダ側の不統一が実測で判明したため、実用は配布先を選ぶ。イントラオンリー系は原理的に不可能。そして mp4 等のコンテナ層では「サンプルを増やさず表示時間だけ伸ばす」という第 3 の道が常に存在するが、stillcast がビットストリーム層を選んだのは「どんなデコーダでも確実に動く最大公分母」という設計意図に基づく。
