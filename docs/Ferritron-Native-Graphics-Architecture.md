# Ferritron Native Graphics（FNG）設計

- 作成: 2026-11-08
- 対象: Ferritron 1.3.1 で新しく作る 2D 描画ライブラリ
- 状態: 設計（実装前）
- 関連: Ferritron 1.3.1 計画の E3「2D 描画とテキスト描画」

---

## 1. 目的

Ferritron Native Graphics（以下 FNG）は、図形・画像・文字を GPU で描く、小さな 2D 描画ライブラリです。

最初の用途は、Ferritron の GPU エディタ（`[view] backend = "gpu"`）です。今は wgpu のサーフェスを渡すだけなので、線・円・角丸矩形・文字をプラグインがそれぞれ自分で用意しています。FNG はこの部分を引き受けます。

ただし FNG は、オーディオプラグイン専用にはしません。**ウィンドウを持つどんな Rust のアプリ**（ツール、ゲームの UI、可視化、スタンドアロンアプリ）からも、そのまま使えるようにします。

### 1.1 満たす要望

PanStudio の要望（「2D 描画とテキスト描画」）の 3 つをすべて満たします。

| 要望 | FNG での対応 |
|---|---|
| (a) OS のフォントを探す API: 言語と太さを渡すと、フォールバックの連なりを返すもの | `fng::fonts`（7.1） |
| (b) テキストのシェイピングとグリフアトラスを wgpu 上で扱うヘルパー | `fng::text` とグリフアトラス（7.2〜7.4） |
| (c) 即時モードの 2D 描画層（パス、塗り、線、テキスト、クリップ、テクスチャへのキャッシュ） | `Scene` と `Renderer`（4〜6 章） |

あわせて、PanStudio が回避策として持っている次のものを置き換えられることを目標にします: tiny-skia と ab_glyph による CPU 描画、3 層の合成、フォントのパスの直書き、簡易的なカーニング・和欧の切り替え。

---

## 2. 設計の原則

1. **汎用**: Ferritron の他のクレートに依存しない。オーディオ、パラメータ、ウィンドウのことを知らない。
2. **軽量**: 依存を少なくする。重い描画ライブラリや UI ツールキット（egui、vello、lyon、tiny-skia、skia）には依存しない。毎フレームの割り当てを、立ち上がりの後はゼロに近づける。
3. **シンプル**: クレートは 1 つ。描画は「シーンを作る → 描く」の 2 段だけ。描き方の種類は少数の基本形に絞る。
4. **マルチプラットフォーム**: Windows / macOS / Linux / iOS / Android / Web（WebAssembly）。GPU は wgpu を通して使い、OS ごとのコードはフォントの探索だけに閉じ込める。
5. **開いた拡張点**: 描画の結果は wgpu のテクスチャか CPU のバッファに出す。ほかの描画（3D、動画、自作のシェーダ）と自由に混ぜられる。UI ツールキットは FNG の上に別の層として作れる。
6. **GPU と CPU で同じ絵**: 図形とグリフの塗り（カバレッジの計算）は CPU と GPU で同じ規則に従う。CPU のバックエンドは、テストの基準の絵と、GPU がない環境の代わりになる。

### 2.1 作らないもの

FNG は「描くこと」だけを受け持ちます。次のものは作りません。

- ウィジェット、レイアウト、入力処理、フォーカス、アクセシビリティ（UI ツールキットの層の仕事）
- ウィンドウの作成とイベントループ（Ferritron では `gpu-view`、ほかのアプリでは winit など）
- 3D、SVG の完全な対応、アニメーションの仕組み
- 文字のサブピクセルアンチエイリアス、ヒンティング、縦書き
- カラー絵文字（COLR / CBDT / sbix）は最初の版では作らない（9 章の後の段階）

---

## 3. 全体の構成

```
アプリ／プラグインのコード
        │  毎フレーム Scene を作る（即時モード）
        ▼
┌──────────────────────── fng（1 つのクレート）────────────────────────┐
│  geom   … 点・矩形・変換・色                                         │
│  path   … パスの組み立てと平坦化                                     │
│  scene  … 描画命令の列（表示リスト）、クリップ、レイヤー            │
│  text   … フォント、シェイピング、改行、テキストレイアウト          │
│  fonts  … OS のフォント探索とフォールバック（feature system-fonts）│
│  raster … CPU のカバレッジ計算（パスとグリフで共用）              │
│  atlas  … グリフとパスのマスクのアトラス                            │
│  render … Renderer：Scene をまとめて描く                            │
│     ├─ wgpu バックエンド（feature wgpu、既定）                      │
│     └─ CPU バックエンド（feature cpu）                              │
└──────────────────────────────────────────────────────────────────────┘
        │                                    │
        ▼                                    ▼
  wgpu のテクスチャ／サーフェス        RGBA8 のバッファ（PNG 化、テスト）
```

- パッケージ名は `ferritron-native-graphics`、ライブラリ名は `fng`（仮。crates.io での重複は公開前に確認する）。
- 置き場所は `crates/ferritron-native-graphics`。Ferritron の他のクレートには依存しない（逆向きの依存だけ）。

### 3.1 feature

| feature | 既定 | 内容 |
|---|---|---|
| `wgpu` | オン | wgpu のバックエンド |
| `cpu` | オフ | CPU のバックエンド（テスト、スクリーンショット、GPU がない環境） |
| `system-fonts` | オン | OS のフォント探索 |
| `bidi` | オフ | 双方向テキスト（アラビア文字・ヘブライ文字）の並べ替え |

`wgpu` も `cpu` もオフにすると、Scene とテキストのレイアウトだけを使える（計測やレイアウトの計算だけを別スレッドで行う用途）。

---

## 4. 主な型と API

### 4.1 基本の型

```rust
pub struct Point { pub x: f32, pub y: f32 }
pub struct Rect  { pub x: f32, pub y: f32, pub w: f32, pub h: f32 }
pub struct Affine([f32; 6]);              // 2D のアフィン変換
pub struct Color { pub r: f32, pub g: f32, pub b: f32, pub a: f32 }   // sRGB、ストレートアルファ

pub enum Paint {
    Solid(Color),
    Linear { start: Point, end: Point, stops: Stops },
    Radial { center: Point, radius: f32, stops: Stops },
    Image  { image: ImageId, transform: Affine, sampling: Sampling },
}

pub struct Stroke { pub width: f32, pub join: Join, pub cap: Cap, pub dash: Option<Dash> }
pub enum FillRule { NonZero, EvenOdd }
```

- 座標は論理ピクセル（DPI に依存しない単位）。実際のピクセルへの倍率は描くときに `Renderer` に渡す。
- `Stops` は最大 16 個の色の止まりを持つ固定長の型（割り当てなし）。

### 4.2 シーン（即時モード）

```rust
let mut scene = fng::Scene::new();           // 使い回す（clear で中身だけ捨てる）

scene.fill_rect(Rect::new(0.0, 0.0, 320.0, 200.0), Paint::Solid(BG));
scene.fill_round_rect(knob_rect, 8.0, Paint::Solid(PANEL));
scene.fill_circle(center, 24.0, Paint::Solid(ACCENT));
scene.stroke_line(a, b, &Stroke::width(2.0), Paint::Solid(FG));
scene.stroke_polyline(&spectrum_points, &Stroke::width(1.5), Paint::Solid(FG));
scene.fill_path(&path, FillRule::NonZero, Paint::Linear { .. });
scene.shadow(card_rect, 12.0, ShadowStyle { blur: 16.0, offset: Point::new(0.0, 4.0), color: SHADOW });
scene.image(image_id, dst_rect);
scene.text(&layout, Point::new(16.0, 24.0), Paint::Solid(FG));

scene.push_clip_round_rect(panel_rect, 8.0);
scene.push_transform(Affine::rotate(angle));
// …
scene.pop_transform();
scene.pop_clip();

scene.push_opacity(0.5);                     // 不透明度のグループ
scene.pop_opacity();

scene.layer(LayerId(1), size, version, |s| { /* 変わらない部分 */ });   // テクスチャにキャッシュ
```

- `Scene` は描画命令を並べるだけで、GPU に触らない。`Send` なので、どのスレッドで作ってもよい。
- `clear()` は中身を捨てて確保した領域を残す。毎フレーム同じ `Scene` を使い回せば割り当ては起きない。
- `layer` は `version` が変わらない限り、前のフレームで描いたテクスチャを使い回す（PanStudio の「3 層の合成」を置き換える）。

### 4.3 描く

```rust
// wgpu を使うアプリ（Ferritron の gpu-view もこれ）
let mut renderer = fng::Renderer::new(&device, target_format);
renderer.render(&device, &queue, &mut encoder, &target_view, &scene, RenderOptions {
    size_px: (w, h),
    scale: 2.0,                        // 論理ピクセル → 実ピクセル
    clear: Some(BG),
});

// CPU（テスト、スクリーンショット）
let mut cpu = fng::CpuRenderer::new();
let image: fng::RgbaImage = cpu.render(&scene, (w, h), 2.0);
image.save_png("out.png")?;
```

- FNG はデバイスとキューを作らない。アプリの `wgpu::Device` / `Queue` と描き先の `TextureView` を受け取るだけなので、ほかの wgpu の描画と同じエンコーダに混ぜられる。
- 画像は `renderer.images().insert(rgba, w, h) -> ImageId` で登録する（CPU 版も同じ API）。

---

## 5. 描画の流れ

```
Scene（命令の列）
   │ 1. 準備（CPU）
   │    ・変換とクリップを適用し、命令をバッチにまとめる
   │    ・パスとグリフを平坦化し、カバレッジを計算してアトラスへ（キャッシュに無いものだけ）
   │    ・頂点／インスタンスのバッファを作る（前フレームの領域を使い回す）
   ▼
GPU へのアップロード（変わった分だけ）
   │ 2. 描画（GPU）
   │    ・基本形：インスタンス化した四角形 + SDF のシェーダ
   │    ・パスとグリフ：アトラスのマスク × 塗り
   │    ・レイヤー：テクスチャへ描いてから合成
   ▼
描き先のテクスチャ
```

### 5.1 描き方の種類（シェーダは 1 つ）

描き方は次の 3 種類だけにし、**1 本のシェーダ**の中で分岐させます。これで描画のバッチを分けずに済み、状態の切り替えが減ります。

| 種類 | 対象 | 方法 |
|---|---|---|
| 解析的な基本形 | 矩形、角丸矩形、円、楕円、線分、折れ線の各区間、影 | 四角形 1 枚をインスタンスとして描き、フラグメントシェーダで符号付き距離（SDF）からアンチエイリアス付きのカバレッジを計算 |
| マスク | 任意のパス、グリフ | CPU で計算したカバレッジ（8 bit）をアトラスに置き、それをサンプリング |
| 画像 | 画像、キャッシュしたレイヤー | テクスチャをサンプリング |

塗り（単色・線形・放射・画像）は、どの種類とも組み合わせられる。グラデーションの色は小さな 1 次元テクスチャにまとめる。

### 5.2 パス

- パスは直線・2 次・3 次ベジェ・円弧で組み立てる。CPU で許容誤差（0.25 実ピクセル）以内の折れ線に平坦化する。
- 塗りのカバレッジは、自作のスキャンライン方式の面積計算（符号付き面積の累積）で求める。非ゼロ規則と偶奇規則に対応。
- 線（stroke）は、線の太さ・つなぎ目・端・破線から輪郭のパスを作り、それを塗る。
- カバレッジはパスと変換の組をキーにしてアトラスにキャッシュする。毎フレーム形が変わるもの（スペクトラムの線など）は、`stroke_polyline` を使えば GPU の解析的な線分で描けるので、CPU の計算が要らない。

### 5.3 色とブレンド

- 色は sRGB で指定する。ブレンドは乗算済みアルファ。
- グラデーションの補間は sRGB 空間（CSS と同じ見た目）。
- 描き先が `*Srgb` 形式でも、そうでなくても同じ見た目になるよう、シェーダの出力で変換する。

### 5.4 クリップ

- 軸に沿った矩形はシザーで切る（最も速い）。
- 角丸矩形はシェーダの SDF で切る。
- 任意のパスでのクリップはマスクのレイヤーで行う（遅いので、文書に注意を書く）。

---

## 6. メモリとパフォーマンス

- **割り当て**: `Scene`、頂点バッファ、インスタンスバッファ、アトラスは使い回す。立ち上がりの後、内容が変わらないフレームでは割り当てをしない（テストで確かめる）。
- **アップロード**: アトラスは変わった矩形だけを書く。
- **アトラス**: グリフとパスのマスクで共用（R8、既定 1024×1024 から必要に応じて 4096 まで拡大。いっぱいになったら、最近使っていないものを捨てる）。
- **描画の呼び出し**: 1 フレームに数回（レイヤーの数 + 1）を目標にする。
- **目標値**（この開発機で測る。値は実装の最初に測ってから確定する）:
  - 基本形 10,000 個 + グリフ 5,000 個の準備が CPU で 2 ms 未満
  - 何も変わらないフレームの準備がほぼ 0（レイヤーのキャッシュが効く）
  - ライブラリ単体のバイナリへの増分を小さく保つ（実装の最初に測って上限を決める）

---

## 7. テキスト

### 7.1 フォントの探索（`fng::fonts`）

```rust
let chain = fng::fonts::query(&FontQuery {
    families: &["Segoe UI", "Hiragino Sans"],   // 希望（無ければ飛ばす）
    generic: Generic::SansSerif,
    weight: 400, italic: false,
    language: Some("ja"),
});   // → フォールバックの連なり（FontId の列）
```

| OS | 方法 |
|---|---|
| Windows | DirectWrite（`IDWriteFontFallback` / `IDWriteFontCollection`） |
| macOS / iOS | CoreText（`CTFontCopyDefaultCascadeListForLanguages`） |
| Linux | fontconfig（実行時に読み込む。リンクは必須にしない） |
| Android | `/system/fonts` と `fonts.xml`（API 29 以上は `AFontMatcher`） |
| Web | OS のフォントは探せない。アプリが登録したフォントだけを使う |

- どの OS でも、フォントのバイト列を直接登録できる（`fonts.register(bytes)`）。アプリに同梱したフォントを最優先にできる。
- フォントのファイルはメモリマップで読み、同じファイルは 1 回だけ読む。

### 7.2 シェイピングと改行

- シェイピング: `rustybuzz`（HarfBuzz の Rust 移植）。カーニング、合字、結合文字、アラビア文字などの複雑な文字に対応。
- フォールバック: 文字のまとまり（クラスター）ごとに、連なりの先頭から順に字形があるフォントを選ぶ。和文と欧文の混在はこれで自動的に切り替わる。
- 改行: `unicode-linebreak`（UAX #14）。日本語の禁則は UAX #14 の範囲で扱う。
- 双方向テキスト: feature `bidi`（`unicode-bidi`）。

### 7.3 テキストのレイアウト

```rust
let layout = fng::text::Layout::new(&mut fonts, "音量 Volume -6.0 dB", &TextStyle {
    font: chain, size: 13.0, line_height: 1.3, align: Align::Start, max_width: Some(120.0),
});
layout.size();          // 計測
layout.hit_test(p);     // 文字の位置（テキスト入力の UI 用）
```

- レイアウトは描画と切り離してあるので、計測だけを先に行える。同じ文字列とスタイルなら結果をキャッシュする。

### 7.4 グリフの描画

- グリフの輪郭は `ttf-parser` で読み、5.2 と同じ CPU のカバレッジ計算でマスクにしてアトラスへ置く（パスと同じ仕組みで、別のラスタライザを持たない）。
- キャッシュのキーは（フォント、グリフ、実ピクセルでの大きさ、小数点以下の位置を 4 段階に丸めた値）。
- 実ピクセルの倍率（DPI）が変わったら作り直す。

---

## 8. マルチプラットフォーム

| 環境 | GPU（wgpu） | フォント探索 | 検証 |
|---|---|---|---|
| Windows | D3D12 / Vulkan | DirectWrite | この開発機で検証する |
| macOS | Metal | CoreText | 型の確認のみ（Mac が手元にない） |
| iOS | Metal | CoreText | 型の確認のみ |
| Linux | Vulkan / GL | fontconfig | 型の確認とテスト（CPU バックエンド）。Ferritron の Linux エディタは未対応のまま |
| Android | Vulkan / GLES | fonts.xml / AFontMatcher | エミュレーターで確認する |
| Web | WebGPU / WebGL2 | 登録したフォントのみ | ブラウザで確認する |

- OS ごとのコードは `fonts` の中だけに置く。描画のコードは全環境で同じ。
- 確かめていない環境は「未検証」と文書に書く。

---

## 9. 依存

| クレート | 用途 | 必須か |
|---|---|---|
| `wgpu` | GPU | feature `wgpu`。版は Ferritron と同じ（現在 30） |
| `ttf-parser` | フォントの読み込み、グリフの輪郭 | 必須 |
| `rustybuzz` | シェイピング | 必須 |
| `unicode-linebreak` | 改行 | 必須 |
| `unicode-bidi` | 双方向テキスト | feature `bidi` |
| `bytemuck` | GPU に渡すデータ | feature `wgpu` |
| OS の API（`windows`、`objc2-core-text` など） | フォント探索 | feature `system-fonts`、OS ごと |
| `png` | PNG の書き出し | feature `cpu` |

上に無い描画ライブラリ・UI ツールキットには依存しない。依存を足すときは、この表に理由を書く。

---

## 10. Ferritron との統合

- `ferritron-gpu-view` に feature `fng` を追加し、GPU エディタから `frame.scene()` で FNG の `Scene` を受け取れるようにする。描画とサーフェスの管理は `gpu-view` が行う。
- DPI（`Ui::scale_factor()`）、描画間隔（`next_frame`）、リサイズは `gpu-view` の値をそのまま `Renderer` に渡す。
- `ferritron-testhost --gui` のスクリーンショットは、今と同じ GPU の読み戻しに加え、CPU バックエンドでも撮れるようにする（GPU がない CI でも比べられる）。
- 素の wgpu の `gpu-view` はそのまま残す。FNG を使わない（vello や自作の描画を使う）プラグインには何も足さない。
- 単体での使用例として `crates/ferritron-native-graphics/examples/`（winit のウィンドウに描く例と、CPU で PNG を書く例）を置く。winit の依存は例だけにとどめる。

---

## 11. テスト

- **基準の絵**: CPU バックエンドで描いた PNG を基準として保存し、変更のたびに比べる（許容差あり）。
- **GPU と CPU の一致**: 同じ Scene を wgpu（この開発機）と CPU で描き、差が許容範囲に収まることを確かめる。
- **割り当て**: 何も変わらないフレームで割り当てが起きないことを、割り当てを数えるアロケータで確かめる。
- **テキスト**: 和欧混在、合字、カーニング、フォールバック、改行、`hit_test` の単体テスト。
- **性能**: 6 章の目標値をベンチマークで測り、退行を見張る。

---

## 12. 開発の段階

| 段階 | 内容 | 終わりの確認 |
|---|---|---|
| M1 | `geom`・`path`・`scene`、CPU のカバレッジ計算、CPU バックエンド | 基本形とパスの基準の絵 |
| M2 | wgpu バックエンド（1 本のシェーダ、インスタンス化、アトラス） | GPU と CPU の一致 |
| M3 | テキスト（`ttf-parser`、`rustybuzz`、改行、レイアウト、グリフのアトラス）と Windows のフォント探索 | 和欧混在の文字が描ける |
| M4 | グラデーション、画像、クリップ、レイヤー、影、破線 | 全機能の基準の絵、割り当てなし |
| M5 | `gpu-view` との統合、`examples/gpu-view` の更新、`testhost --gui` | PanStudio の `canvas.rs` / `gpu.rs` / フォントパスの直書きを置き換えられることの確認 |
| M6 | macOS / iOS / Linux / Android / Web のフォント探索とビルド | Android と Web で動作、ほかは型の確認 |

---

## 13. 決めていないこと

1. パッケージ名とライブラリ名（`ferritron-native-graphics` / `fng`）の最終決定と、crates.io での重複の確認。
2. バージョンの付け方: 今は Ferritron の全クレートと同じ版（1.3.1）。汎用ライブラリとして単独で公開するときに、版を分けるかを決める。
3. カラー絵文字（COLR v0/v1、CBDT）の対応の時期。
4. 6 章の性能目標の具体的な値（M1 で測ってから決める）。
