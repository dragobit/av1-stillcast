# VP9 の AV1 4 層モデル対応 — syntax unit / grouping / serialization / container

> 前書き: stillcast の設計では AV1 ビットストリームを 4 層で捉える( `docs/transport-io.md` )。本文書は同じ分類を VP9 に写像したものである。最大の発見は **VP9 には層が実質 1 つ少ない**こと——AV1 の OBU という「型付きユニットの入れ物層」が VP9 には存在せず、syntax unit と grouping が「1 圧縮フレーム」に縮退している。serialization では「AV1 の低オーバーヘッド OBU ストリーム」に相当するものが IVF で、「Annex-B」に相当する規格化直列化は VP9 には存在しない。container では IVF がまさに VP8/VP9 のために作られた「ネイティブ」フォーマットであり、さらに WebM が VP9 のリッチコンテナとして存在する。
>
> 数値・挙動は本マシンで検証済み(ffmpeg 6.x, libvpx)。

## エグゼクティブサマリ

| AV1 の層 | VP9 での相当物 | 差分 |
|---|---|---|
| 1. syntax unit = OBU(header + payload) | **圧縮フレーム**(非圧縮ヘッダ + 算術符号化ペイロード) | VP9 には「型タグ+サイズの入れ物」がない。メタデータ系ユニット(TD/seq hdr/metadata OBU)も存在せず、全情報はフレーム非圧縮ヘッダに内包 |
| 2. grouping = TU(1 デコード/表示ステップの OBU 列) | **1 圧縮フレーム = 1 デコード/表示ステップ**(ほぼ同一概念に縮退) | TU という名前の層はない。複数フレームを束ねる唯一の機構が superframe index |
| 3. serialization = 低オーバーヘッド OBU ストリーム \| Annex-B | **IVF**(フレーム毎 12 B 区切り)が「低オーバーヘッド OBU ストリーム」相当。**Annex-B 相当は存在しない** | 生の圧縮フレーム連結は自己区切りを持たず、ファイルとして単独では読めない(実証済み) |
| 4. container = IVF / MP4 / MKV | **IVF**(VP8/VP9 生まれのフォーマット、まさにネイティブ) / **WebM**(V_VP9、Matroska サブセット) / **MKV** / **MP4**(vp09 タグ) | VP9 の真のリッチコンテナは WebM。IVF はビデオのみ。mp4 では vp09 codec タグで登録 |

つまり VP9 は「AV1 より 1 層少ない世界」であり、stillcast が AV1 で行っている OBU 層の作業(TD 挿入、seq header 正準化、uheader 手術)は VP9 では**層自体が存在しないため原理的に不要**になる。この単純さは前稿 [cross-codec-two-stage.md](cross-codec-two-stage.md) の「VP9 は純粋複製で最も容易」という結論を構造面からも裏付ける。

---

## 第 0 層(前提)— ビット読み出し規約の違い

4 層の手前の層として、**ビットの読み出し規約**が AV1 と VP9 で違う点を押さえておくと後の説明が明瞭になる。

- **AV1**: フィールドは MSB ファーストのビット列で記述され、サイズ付き整数には `leb128`(リトルエンディアン基底 128)を使う。OBU ヘッダは先頭 1 バイトで `forbidden_bit + type(4) + ext_flag + has_size + reserved` が決まる。
- **VP9**: 非圧縮ヘッダも MSB ファーストで読む(各バイトの最上位ビットから順にフィールドを取る)。ただし多バイト整数は `L(n)` 関数=リトルエンディアンで読み、圧縮ペイロード側は boolean coder(範囲符号化)という別の読み方になる。つまり **1 フレーム内でビット順・整数エンディアン・符号化方式が 3 系統混在**する。

実例: 実測した keyframe 先頭バイト `0x82`(= `1000 0010`)を MSB ファーストで読むと `frame_marker=0b10, profile=00, show_existing=0, frame_type=0(KEY), show_frame=1` となり、続く `49 83 42` の同期コードと整合する。同様に stillcast 検証で作った SE パケット `0x88`(= `1000 1000`)は MSB ファーストで `frame_marker=10, profile=00, show_existing=1, idx=000` と読め、これが VP9 の最小表示指示ユニット(1 バイト)として成立する根拠である。

---

## 第 1 層 syntax unit — OBU(型+ペイロード)に相当するもの

### AV1 側

AV1 の構文は OBU(Open Bitstream Unit)という自己記述ユニットの列である。各 OBU は `obu_type(4bit)+extension_flag+has_size_field` のヘッダと、leb128 のサイズフィールド、ペイロードから成り、型が 15 種ある(SEQUENCE_HEADER, TEMPORAL_DELIMITER, FRAME_HEADER, TILE_GROUP, FRAME, REDUNDANT_FRAME_HEADER, METADATA, PADDING, …)。**「コンテンツでないユニット」(時間区切り・設定・メタデータ)がコンテンツと同じ入れ物で混在する**のが AV1 の特徴であり、stillcast はこの入れ物を直接操作する。

### VP9 側

VP9 には OBU に相当する「型付きユニット」が**存在しない**。ビットストリームの最小の意味単位は**圧縮フレーム(compressed frame)**で、これは

```
[非圧縮ヘッダ(ビット列、バイト境界非整列の変長)]
[算術符号化ペイロード(first partition + 各 tile partition)]
```

の 2 部で構成される **1 個のバイト列**である。「このバイト列が何型のユニットか」を示す外側のタグはなく、中身の第一フィールド `frame_marker=10` がフレーム開始を示すだけである。ユニット内にサイズフィールドもないため、**フレーム自体は自分の終端を知らない**——これが後述する serialization 問題の根である。

### フィールド対応 — AV1 の「コンテンツでない OBU」は VP9 ではどこへ行ったか

| AV1 の OBU/フィールド | VP9 での所在 |
|---|---|
| SEQUENCE_HEADER OBU(解像度・プロファイル・色・ operating points) | **なし**。これらの情報は**キーフレームの非圧縮ヘッダ内**に埋め込まれる(同期コード `0x49 0x83 0x42`、color config、frame size)。つまり VP9 は「キーフレームがシーケンスヘッダを兼ねる」設計 |
| TEMPORAL_DELIMITER OBU(TU 区切り) | **なし**。TU という概念自体がないので区切りも不要 |
| FRAME_HEADER OBU / FRAME OBU | 圧縮フレームの非圧縮ヘッダ部そのもの |
| TILE_GROUP OBU | 圧縮フレームの tile partition 部(フレーム内に内包) |
| METADATA OBU(HDR CLL/MDCV, scalability, timecode) | **なし**。VP9 にビットストリーム内メタデータ機構はない。HDR/色情報はキーフレームヘッダの color config とコンテナ層(WebM の Colour element, mp4 の colr)で運ぶ |
| PADDING OBU | なし(必要にならない設計) |
| show_existing_frame フィールド | **存在する**: 非圧縮ヘッダの `show_existing_frame(1 bit)+frame_to_show_map_idx(3 bit)`。**AV1 とほぼ同じ表示プリミティブ**で、しかも showable 制約がない(前稿参照) |

### 圧縮フレーム内の構造(詳細)

VP9 非圧縮ヘッダの主なフィールド(キーフレーム/インターフレーム共通部):

```
frame_marker               f(2) = 0b10
profile_low_bit            f(1)
profile_high_bit           f(1)
show_existing_frame        f(1)   ← 表示プリミティブ
[if show_existing]: frame_to_show_map_idx f(3) → 終了(1 バイト収まること多い)
frame_type                 f(1)   (0=KEY, 1=INTER)
show_frame                 f(1)   (0=非表示フレーム=alt-ref 相当)
error_resilient_mode       f(1)
[KEY の場合]: frame_sync_code 24bit = 0x49 0x83 0x42, color config, frame_size w/h
[INTER の場合]: intra_only, reset_frame_context, refresh_frame_flags(8bit),
                ref_frame_idx(3bit)×3, ref_frame_sign_bias×3, allow_high_precision_mv
refresh_frame_context, frame_parallel_decoding_mode, frame_context_idx
loop_filter パラメータ, quantization パラメータ(delta q),
segmentation パラメータ, tile info(cols/rows), first partition size
```

ここから分かる重要な違い: AV1 の OBU モデルでは「フレームの表示」「参照の更新」「時間区切り」がユニット型として分離されているのに対し、**VP9 ではそれらすべてが同じ非圧縮ヘッダのビットフィールドとして同居する**。`refresh_frame_flags` が「このフレームが DPB スロットを更新するか」を、`show_frame` が「このフレームが表示されるか」を、それぞれヘッダ内フラグで担う。型付きユニットとして層を分ける必要がなかった設計選択である。

### superframe index(準構文ユニット)

VP9 には「superframe」という、**1 パケット(= コンテナの 1 サンプル)内に複数フレームを束ねる**機構がある。ペイロード末尾に次の index が付く:

```
[frame0 bytes][frame1 bytes]...[frameN-1 bytes][index]
index = [各フレームのバイト長 u32 × N][marker 0xC0 | (N-1)<<3 | (size_bytes-1)]
```

デコーダ/パーサは末尾から index を読み、内部フレームを分割する。libvpx は非表示フレーム(alt-ref や `show_frame=0` の更新フレーム)を直後の表示フレームと同じパケットに束ねるために使う。stillcast の SE パケットのような表示指示は superframe に束ねる必要はなく、1 パケット = 1 表示フレームのまま運べる。

### 圧縮ペイロードの内部(first partition / tile partitions)

「1 圧縮フレーム」のペイロード側も、AV1 の TILE_GROUP に相当する構造を持つ:

- **first partition**: 算術符号化(boolean coder)の第 1 区画で、フレーム全体に共有される予測パラメータ(MV 確率、フィルタ係数、参照確率、係数確率の更新情報等)を運ぶ。非圧縮ヘッダ内の `first_partition_size` フィールド(16 bit)がその長さを示す——**フレーム内で唯一のサイズフィールドであり、これがフレーム終端ではなく内部区画の境界**を示す点が重要。
- **tile partitions**: `tile_cols`/`tile_rows` で分割された空間タイル毎の算術符号化データ。tile 列毎に独立にデコード可能なので、マルチスレッドデコードの単位になる。AV1 の TILE_GROUP/TILE に相当するが、VP9 ではこれらは 1 フレーム内に内包され、独立ユニットとして外に出ない。

つまり VP9 では「ペイロードをさらにユニットに割る」層はフレーム内部に閉じており、ストリーム構造のレベルでは見えない。OBU のように「TILE_GROUP だけ別ユニットで並べ替える」ような操作は原理的にできない設計である。

### VP9 の参照モデル(8 スロット + 3 論理参照)

DPB(Decoded Picture Buffer)の扱いは AV1 と似ている: 8 個の参照スロットを持ち、フレーム非圧縮ヘッダの `refresh_frame_flags`(8 bit)が「このフレームのデコード結果をどのスロットに入れるか」を指定する。各インターフレームはさらに `ref_frame_idx[3]` と `ref_frame_sign_bias[3]` で LAST / GOLDEN / ALTREF の 3 論理参照をスロットに割り当てる。

- **keyframe**: `refresh_frame_flags` に関わらず全 8 スロットを自身でリフレッシュする(= AV1 KEY_FRAME の挙動と同じ、DPB の完全リセット)。これが「keyframe のバイト複製が毎 GOP で合法」な根拠。
- **show_existing_frame**: `frame_to_show_map_idx` で指したスロットの絵を再表示するだけで、DPB を一切更新しない(デコードする画素データがない)。
- **stillcast 観点の違い**: AV1 では `showable_frame` 制約で keyframe を再表示できないが、VP9 にはその制約がないため、**DPB リセット源の keyframe 自体を繰り返し表示の対象にできる**。

---

## 第 2 層 grouping — TU(1 デコード/表示ステップ)に相当するもの

### AV1 側

TU(Temporal Unit)は「1 回のデコード/表示ステップに必要な OBU 列」で、TD OBU から次の TD OBU までが 1 TU。AV1 は TU の中に FRAME_HEADER、TILE_GROUP、REDUNDANT_FRAME_HEADER 等を複数持ち得るし、show_existing だけの TU(= stillcast の SE TU)もある。

### VP9 側

VP9 では **1 圧縮フレーム = 1 デコード/表示ステップ** であり、grouping 層はほぼ syntax unit と同一視される。「TU」に相当する名前の層はない。ただし「1 コンテナサンプルの中に複数のデコード/表示ステップ」を実現するための superframe が、弱い grouping 相当として存在する(前項)。

この層が薄いことは stillcast 観点で重要で、AV1 で必要だった「TU 境界管理・TD OBU 挿入・REDUNDANT_FRAME_HEADER の扱い」は **VP9 では作業が存在しない**。シーケンス構造は「keyframe または inter frame の圧縮フレームの列」だけであり、TU 区切りもシーケンスヘッダもないため、ストリームを「何も挟まずただフレームを並べる」だけで構成できる。

### show_frame=0 のフレームと stillcast との関係

VP9 の「非表示フレーム(show_frame=0)」はデコードするが表示しないフレームで、alt-ref や「ゴールデン相当の参照フレームを後から出す」目的に使われる。stillcast の AV1 における golden はこの概念に近い(実際 golden は `show_frame=0` でキャッシュされる)。VP9 ではこの非表示フレームがそのまま「表示指示の素材」として使える上、`show_existing_frame` が showable 制約なしで任意スロットを指せるため、**golden 相当の別素材すら不要**で keyframe 1 枚で済む(前稿の実証: KF + `0x88` パケットの複製で成立)。

---

## 第 3 層 serialization — 低オーバーヘッド OBU ストリーム / Annex-B に相当するもの

### AV1 側(復習)

AV1 には 2 系統の直列化がある:

- **低オーバーヘッド OBU ストリーム**: OBU をそのまま並べたもの。各 OBU は leb128 のサイズフィールドを持つので自己区切りが可能。IVF や mp4 内ではこの形を使う。
- **Annex-B**: `temporal_unit_size / frame_unit_size / obu_length` を明示的な長さ接頭辞で付けるストリーム形式。TS や生ビットストリーム用途で規格化されている。

両者とも規格が存在し、どちらも「コンテナなしで単体で読める素のエレメンタリストリーム」である。

### VP9 側 — 直列化の事情は根本的に違う

VP9 の圧縮フレームは**自分の終端を知らない**(前述: サイズフィールドを持たない)。したがって、

- **「生の圧縮フレーム列」はファイルとして成立しない**: 連結しただけではフレーム境界が分からず、再分割もできない(実証: IVF からヘッダを剥いだ 30,022 B の生連結ファイルを `ffprobe` に渡すと `Invalid data found when processing input` で拒否された)。つまり **AV1 の「低オーバーヘッド OBU ストリーム」に相当する「自己区切りの素列」は VP9 には存在しない**。
- **Annex-B 相当も存在しない**: VP9 には「TU 長さ+フレーム長さ」を付けた規格直列化形式が標準化されていない。superframe index が近い概念だが、あれは「パケット内の複数フレーム束ね」機構であり、ストリーム全体の長さ接頭辞形式ではない。

そのかわり、実務的に「serialization 相当」を担うのは次の 2 つである:

1. **IVF**: フレーム毎に 12 B のヘッダ(`u32 size + u64 timestamp`)を付けてフレーム境界を作る軽量形式。**「低オーバーヘッド OBU ストリーム」に相当するのが実質これ**であり、VP9 にとって最小の自立直列化フォーマットである。
2. **コンテナのサンプル境界**: WebM/MKV/mp4 では、各サンプル(= 1 圧縮フレーム or superframe)の長さがコンテナ側の要素で管理されるので、エレメンタル側は区切り不要。サンプル=フレームの 1:1 対応が基本。

### pipe して保存できるか — ffmpeg での実測

「serialization を pipe して保存できるか」は利用側の実務問題である。本機で検証した結果:

| 形式 | ffmpeg の pipe 出力 | 結果 |
|---|---|---|
| IVF | `ffmpeg -i in.ivf -c:v copy -f ivf pipe:1 > out.ivf` | **可能**。非シーク可能出力では IVF ヘッダのフレーム数フィールドが `0xFFFFFFFF` センチネルになるが、ffprobe は全 60 フレームを正しく数え、デコードも問題ない(実証) |
| WebM | `ffmpeg -i in.ivf -c:v copy -f webm pipe:1 > out.webm` | **可能**。WebM はストリーミング設計で、非シーク出力も正常 |
| MKV | `ffmpeg -i in.ivf -c:v copy -f matroska pipe:1` | 可能(WebM と同じ Matroska 系) |
| mp4 | `ffmpeg -i in.ivf -c:v copy -f mp4 pipe:1` | **不可のまま**。moov アトムを末尾に書くため、非シーク出力では標準では書けない。`-movflags frag_keyframe+empty_moov` で fragment mp4 化すれば pipe 可能になる |

**保存拡張子の実務**:
- `.ivf` — ビデオのみの素コンテナ。最小・最軽量、フレーム境界が確��、時刻刻みも保持。stillcast 系処理の素直列化として最有力。
- `.webm` — VP9 の本来のリッチコンテナ。音声(Vorbis/Opus)、シーク、メタデータ可。pipe/ストリーミング適性あり。
- `.mkv` — Matroska。webm より広い codec/機能対応。
- `.mp4` — vp09 codec タグで格納可(本機で自動タグ付け確認)。HLS/DASH や一般プレーヤ互換で使う。pipe には断片化フラグが要る。
- `.vp9` — 生フレーム連結。**実用上不可**(再分割できないため ffmpeg の demuxer も存在しない)。避けるべき。

### バイトレコードのレイアウト対照

serialization 層の違いを実バイトで対照する。

```
AV1 低オーバーヘッド OBU ストリーム:
  [OBU header][leb128 size][payload] [OBU header][leb128 size][payload] ...
   └ OBU 自身がサイズを内包するので自己区切り

AV1 Annex-B:
  [temporal_unit_size][frame_unit_size][obu_length][payload] ...
   └ 外側の長さ接頭辞で区切る規格形式

VP9 生フレーム列:
  [frame bytes][frame bytes][frame bytes] ...
   └ サイズも終端マーカーもないため、単体では読めない(実証済み)

VP9-in-IVF(serialization 相当):
  [DKIF 32B ヘッダ]
  [u32 size][u64 timestamp][frame bytes] ...
   └ 外側(IVF レコード)がフレーム境界を作る

VP9-in-WebM/MKV:
  [SimpleBlock: track + timecode + flags + frame bytes] ...
   └ EBML 要素の長さがフレーム境界を作る

VP9-in-MP4:
  [sample](stsz/stco でサイズと位置を管理)
   └ moov 側のテーブルがフレーム境界を作る
```

ポイントは、VP9 では**フレーム境界を作る責任が常に外側の層にある**ことだ。AV1 では OBU が自分の長さを持つのでストリームが自己完結するが、VP9 では IVF・WebM・mp4 のいずれかの「区切りを提供する外側」が常に必要になる。

### タイミング(時刻)の扱い

AV1 では TD OBU や operating_parameters の decoder model が「いつ」表示するかの情報をエレメンタル層に置ける。VP9 にはこれに相当する機構が**エレメンタル層には一切なく**、フレームの表示時刻は完全に外側(IVF の u64 timestamp、WebM の SimpleBlock timecode、mp4 の stts/pts)が担う。

したがって stillcast 系処理で「任意の fps/長さ」を実現する場合、VP9 ではエレメンタル層に何も書き込む必要がなく、**コンテナ/IVF 層が時刻表を管理するだけ**でよい。実際、前稿の実検証では `0x88` パケットにタイムスタンプを一切書かず、IVF 側の timestamp カウンタだけで正しい時刻列を構成できた。

### デコーダ API から見たフレーム境界の責任者

「フレームが自分の終端を知らない」という特性は、デコーダ API の設計にも表れている。libvpx のデコード入口は `vpx_codec_decode(ctx, data, data_sz, user_priv, deadline)` で、**呼び出し側がフレームのバイト列とその長さを渡す**。つまり libvpx は「フレーム境界を外側から与えられる」ことを前提にしており、ストリーム内で境界を見つける責任はデコーダにない。AV1 の dav1d 系も同じく「TU/OBU の切れ目を外から与える」API 設計だが、OBU は自分のサイズを持つため内部で再分割できた。VP9 ではこの「外から与える」ことが必須で、それが IVF のような区切りフォーマットが必要な理由である。

### error_resilient_mode とフレーム並列性

VP9 非圧縮ヘッダの `error_resilient_mode` フラグは「このフレームが前フレームの算術符号化コンテキストや確率状態を引き継がない」ことを示す。`frame_parallel_decoding_mode` と併せて、デコーダがフレーム毎に確率状態をリセットできるかを制御する。stillcast 式の複製配置では、**前フレームが SE パケット(= 画素データを持たない)のとき、直後の実フレームがエントロピー状態を引き継がないことが望ましい**。AV1 では decoder model の修復で対応した「フレーム間の暗黙依存」に相当する関心事が、VP9 ではこの 1 ビットで制御できる。現状の検証は全て show_existing チェーンのみだったため、この辺りは実装上の注意点である(付録 D 参照)。

### superframe の実例

libvpx は `VP9E_SET_ENABLE_AUTO_ALT_REF` 等で alt-ref を有効にしたとき、非表示フレーム(alt-ref)を直後の表示フレームと同じ出力パケットに束ねる。この「1 パケット内に複数フレーム」を可能にするのが superframe index で、ffmpeg の VP9 パーサ(`ff_vp9_parser`)が末尾 index を読んで内部フレームを分割する。stillcast の SE パケットは単独フレームとして運べるので superframe に束ねる必要はないが、「同じタイムスタンプで複数のデコードステップを起こす」仕組みとして知っておく価値がある。

**ffmpeg での各種操作例(実測可能コマンド)**:

```bash
# encode → IVF(= serialization 層をファイルに保存)
ffmpeg -loop 1 -i image.png -c:v libvpx-vp9 -crf 36 -b:v 0 -frames:v 60 out.ivf

# remux: IVF → WebM / MKV / MP4
ffmpeg -i in.ivf -c:v copy out.webm
ffmpeg -i in.ivf -c:v copy out.mkv
ffmpeg -i in.ivf -c:v copy out.mp4          # vp09 タグが自動で付く

# pipe → IVF ファイル(非シーク出力も有効)
ffmpeg -i in.webm -c:v copy -f ivf pipe:1 > out.ivf
ffmpeg -i in.ivf  -c:v copy -f webm pipe:1 > out.webm
```

なお、本機の ffmpeg には **VP9 の生 demuxer/muxer は存在しない**(`-f vp9` は使えない)——これも「VP9 のフレームは自己区切りを持たない」ことの当然の帰結である。

---

## 第 4 層 container — IVF/MP4/MKV に相当するもの

### 「VP9 の IVF 相当」という問いへの答え

問いの形は「AV1 の IVF のような VP9 ネイティブコンテナはあるか」だが、実は **IVF 自体が VP8/VP9 のために作られたフォーマットである**。IVF(Indeo/Intel Video Format、別名 "On2 IVF" や DKIF)は On2/Google が VP8 の生ストリームを格納するために設計したもので、その後 VP9(`VP90` fourcc)、AV1(`AV01`)、VVC(`VV01`)にも fourcc を変えて流用された。つまり AV1 が「ネイティブ素コンテナ」として使っている IVF は、実は VP8/VP9 生まれの流用であり、**VP9 にとって IVF は真正のネイティブ**である。

IVF ヘッダ(32 B)の実例(本機で生成した vp9.ivf):

```
44 4b 49 46  "DKIF"
00 00        version = 0
20 00        header size = 32
56 50 39 30  "VP90"         ← fourcc(コーデック識別子)
80 02 80 02  width=640, height=640
1e000000 01000000  framerate=30, timescale=1
3c000000              frame count=60(ファイル書きなら確定値)
00000000              unused
```

フレームごとのヘッダは 12 B(`u32 フレームバイト数 + u64 タイムスタンプ`)。シンプルかつ「フレーム境界と時刻」だけを提供する、AV1 の「低オーバーヘッド OBU ストリーム」にあたる位置づけである。

### もう一つのネイティブ: WebM

VP9 が産業的に最も使われるコンテナは WebM である。WebM は Google が VP8/VP9 + Vorbis/Opus のために Matroska をサブセット化した規格で、VP9 の codec ID は `V_VP9`。音声・シーク・字幕・メタデータ・チャプター・ストリーミングに対応する VP9 の真正リッチコンテナであり、YouTube やブラウザ再生での実質標準である。

Matroska(.mkv)は WebM の親で、機能的に上位互換。mp4 では ISO/IEC 登録の `vp09.xx.yy.zz` codec タグで格納される(`-tag:v vp09`、実測で自動付与も確認)。

### 各コンテナの機能対応表

| コンテナ | 映像 | 音声 | シーク | ストリーミング | 備考 |
|---|---|---|---|---|---|
| IVF | VP9 のみ | ✗ | ○(フレーム単位) | ○(pipe 可、count センチネル) | 最小。生エレメンタルのデファクト |
| WebM | V_VP9 | Vorbis/Opus | ○ | ◎(設計がストリーミング) | VP9 のネイティブリッチコンテナ |
| MKV | V_VP9 | ほぼ全て | ○ | ○ | WebM の親、機能最上位 |
| mp4 | vp09 | AAC/Opus 等 | ○ | △(要フラグ) | 一般互換性・DASH 用 |

### コンテナ層が担うもの(AV1 側と同じ)

タイミング(stts/pts)、音声複合、シークインデックス(stss/cues)、メタデータ(HDR/色、タイトル等)はすべてコンテナ層の仕事であり、VP9 のエレメンタル層はこれらを一切持たない。これは AV1 と同じ責務分担である。

### IVF の限界と位置づけ

IVF は「DKIF ヘッダ + (size, timestamp) のフレームレコード列」という最小構造しか持たず、**音声トラック・シークインデックス・メタデータ・複数トラックを一切サポートしない**。したがって IVF は「エレメンタルストリームをファイルに落とすための最小容器」であり、配布・再生用の最終形ではなく中間フォーマットとして使うのが自然である(stillcast も AV1 側で同じ使い分けをしている)。最終配布には WebM か mp4 が必要になり、音声を伴う映像(音楽トラック+静止画等)は必然的に IVF から WebM/mp4 への remux が必要になる。この「serialization=IVF、最終配布=WebM/mp4」という役割分担は AV1 の場合と構造的に同じである。

---

## 第 5 層(補遺) — AV1 にあって VP9 にない層

4 層分類の外で、AV1 にはあるが VP9 には存在しないものを挙げる:

- **decoder model / HRD**: AV1 の `operating_parameters`(buffer removal time, delay 等)は unpadded header 手術の対象になるほどの構造を持つが、VP9 にはこの層がない。したがって stillcast 移植では **uheader 修復という作業が存在しない**。
- **TU 境界(TD OBU)**: 存在しない。
- **シーケンスヘッダ OBU**: 存在せず、キーフレームヘッダが兼務。
- **メタデータ OBU**: 存在せず、HDR/色はコンテナ層が担当。
- **Annex-B 規格直列化**: 存在せず、素列は自己区切りを持たない。

逆に VP9 だけにあるもの:

- **superframe index**: 1 パケット内の複数フレーム束ね。AV1 にはこの仕組みがない(AV1 は TU で別手段)。
- **`refresh_frame_flags` の 8 スロット同時更新指示**: AV1 も同様だが、VP9 では keyframe が「全スロット自身更新」を強制する動作がより直接的。

### stillcast 移植観点でのまとめ

VP9 の層構造は AV1 の真のサブセットに近い。**層が 1 つ少ない = 触るべき構文が少ない**。stillcast が AV1 でやっている「TD 挿入・seq header 正準化・decoder model 手術・golden 採取」はすべて消え、残るのは「keyframe パケット採取 + 定数 1 B の show_existing パケット複製 + IVF/WebM への格納」だけになる。これは [cross-codec-two-stage.md](cross-codec-two-stage.md) の結論(VP9 が Tier1 本命)を構造レベルで裏付ける。

stillcast の既存機構の写像表:

| stillcast/AV1 の機構 | VP9 相当 | 要否 |
|---|---|---|
| `src/frame_header.rs` の OBU フィールド解析 | 非圧縮ヘッダのビットフィールド解析(先頭数ビットで frame_type/show_frame/show_existing/refresh_flags) | 要: ただし解析対象は 1 フレームの先頭数バイトのみで、AV1 の TU 横断解析より小さい |
| `split_input` の anchor(シーケンスヘッダ+shown KF)検出 | 「keyframe パケット」の検出(frame_type=KEY + show_frame=1)。シーケンスヘッダ相当は同じパケット内の非圧縮ヘッダに内包されるので別ユニットを探す必要がない | 要 |
| `split_input` の golden(showable 非キー、order_hint 連続)検出 | **不要**(show_existing が keyframe を直接指せる) | 消える |
| `show_existing_tu()`(TD+FH OBU 合成) | `0x88` 等の 1 バイト SE パケット生成 | 要: 生成対象が 6 B → 1 B に縮小 |
| `unheader` 手術(operating_parameters 復号モデルフラグ) | **なし**(decoder model 層が存在しない) | 消える |
| seq header 正準化(セグメント間バイト一致) | keyframe 非圧縮ヘッダの一致確認のみ(別ユニットではない) | 緩和 |
| TU 連結・TD 境界管理 | **なし**(TU がない) | 消える |
| stts/stss 生成(mp4) | 同じ | 要 |
| Annex-B 直列化選択 | **なし**(選択肢自体がない) | 消える |
| IVF 出力 | IVF 出力(VP90 fourcc) | 同じ |
| WebM 出力 | WebM 出力(V_VP9) | 同じ |

「消える」作業が多いのは、AV1 でそれらが必要だった理由が「OBU 層・TU 層・decoder model 層」という VP9 に存在しない層に起因するためである。残るのは「素材パケットの採取と複製」と「コンテナ格納」の 2 作業だけで、これは [cross-codec-two-stage.md](cross-codec-two-stage.md) の「VP9 は純粋複製 + 1 ユニット契約」と一致する。

---

## 付録 A. 検証コマンド

```bash
# 素材生成(VP9 60 フレーム)
ffmpeg -y -loop 1 -i jacket.png -vf format=yuv420p -r 30 -frames:v 60 \
  -c:v libvpx-vp9 -crf 36 -b:v 0 vp9.ivf

# IVF ヘッダ確認(DKIF + VP90 fourcc + 640x640 + 30fps + 60f)
xxd -l 32 vp9.ivf

# pipe → IVF(非シークでも書ける、count=0xFFFFFFFF でも読める)
ffmpeg -y -i vp9.ivf -c:v copy -f ivf pipe:1 > piped.ivf
ffprobe -count_frames piped.ivf   # 60 frames

# 生連結ファイルは読めないことを確認
python3 -c "..." # IVF からフレームを剥がして連結
ffprobe raw_concat.vp9            # Invalid data found

# remux 各種
ffmpeg -i vp9.ivf -c:v copy out.webm   # WebM
ffmpeg -i vp9.ivf -c:v copy out.mkv    # MKV
ffmpeg -i vp9.ivf -c:v copy out.mp4    # vp09 タグ
```

## 付録 B. 前提知識

- 圧縮フレーム(compressed frame): VP9 の最小構文単位。非圧縮ヘッダ+算術符号化データ。
- superframe: 複数圧縮フレームを 1 パケットに束ねる機構。末尾に index。
- IVF: On2/Google 製の素コンテナ。VP8/VP9 生まれ、AV1/VVC も流用。
- WebM: Matroska のサブセット。VP8/VP9 のリッチコンテナ。
- vp09: mp4 内の VP9 codec タグ(ISO/IEC 登録、`vp09.profile.level.bitDepth...`)。
- show_existing_frame: DPB の参照フレームを再表示する指示。VP9/AV1 両方に存在。

## 付録 C. 実測バイトダンプ

実際に生成した `vp9.ivf` の IVF レコード先頭(フレーム 0 とフレーム 1):

```
IVF ヘッダ(32 B):
  44 4b 49 46        "DKIF"
  00 00              version=0
  20 00              header_size=32
  56 50 39 30        "VP90"
  80 02 80 02        width=640, height=640
  1e000000 01000000  framerate=30, timescale=1
  3c000000           frame_count=60
  00000000           unused

フレーム 0 レコード(12 B ヘッダ + 28,483 B ペイロード):
  43 6f 00 00        size = 0x6f43 = 28,483
  00..00 (8B)        timestamp = 0
  ペイロード先頭 16 B: 82 49 83 42 00 27 f0 27 f6 12 38 24 1c 18 66 10
    └ 0x82 = frame_marker=0b10, profile=00, show_existing=0, frame_type=0(KEY), show_frame=1
      0x49 0x83 0x42 = frame_sync_code(キーフレームの固定マーカー)

フレーム 1 レコード(12 B ヘッダ + 31 B ペイロード):
  size=31, timestamp=1  — 静止画面なので 2 フレーム目以降は全スキップの小さな P フレーム
```

ここから読み取れること:
- **キーフレームは「シーケンスヘッダ兼務」**: `0x82` の非圧縮ヘッダ先頭に同期コードと色・サイズ情報が入り、AV1 の SEQUENCE_HEADER OBU の別立てがない。
- **フレーム 1 以降は 31 B**: 静止画像の後続フレームが「ほぼ全スキップ」で小さいのはエンコーダ自然出力でも確認できるが、stillcast 式ではこの役割を `0x88` の 1 バイト SE パケットが担う。
- **フレーム境界は IVF レコードが作る**: ペイロード内に自分の終端を示すフィールドはない。

## 付録 D. 未検証・残課題

- libvpx が実際に superframe を発行する条件(alt-ref 使用時や `show_frame=0` フレームの直後)は未検証。
- WebM の Colour element(色空間・転送特性)と mp4 の colr の具体的な登録値は未確認。
- IVF の非シーク出力で `0xFFFFFFFF` センチネルを正しく扱うデコーダ実装の範囲(ffprobe は受理、他実装は未検証)。
- VP9-in-MP4 の `vp09.xx.yy.zz` codec タグのフィールド値の算出ルール(profile/level/bitDepth/chromaSubsampling 等)は仕様書参照のみ。

---

## 付録 E. AV1/VP9 の層対応早見表(再掲)

| 層 | AV1 | VP9 |
|---|---|---|
| syntax unit | OBU(type+size+payload の自己記述ユニット) | 圧縮フレーム(型タグなしの 1 バイト列) |
| grouping | TU(TD 区切りの OBU 列) | (なし)1 フレーム = 1 ステップ。superframe が弱い束ね |
| 設定/時間ユニット | SEQ_HDR OBU / TD OBU / operating_parameters | (なし)KF 非圧縮ヘッダに内包 / 時刻はコンテナのみ |
| serialization | 低オーバーヘッド OBU ストリーム, Annex-B | **IVF** (生連結は不可), コンテナ内サンプル境界 |
| container | IVF(AV01) / MP4 / MKV | IVF(VP90) / WebM(V_VP9) / MKV / MP4(vp09) |
| 表示指示 | show_existing_frame OBU(~6 B/TU) | show_existing_frame ビット(1 B パケット) |
| 参照更新 | refresh_frame_flags + showable 制約 | refresh_frame_flags(8 スロット、制約なし) |

*本稿は [cross-codec.md](cross-codec.md)(最終生成物の可能性) と [cross-codec-two-stage.md](cross-codec-two-stage.md)(二段階処理の適用性) の姉妹文書で、VP9 の構造対応を扱う。検証環境: ffmpeg 6.x, libvpx, Ubuntu。*
