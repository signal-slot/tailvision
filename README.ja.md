# tailvision

AI エージェントに「実機の画面」を見せ、「指」を貸す装置。

> **状態 (2026-10-07):** 開発中。ビルド・単体テスト・SD イメージ生成までは
> 動くが、実機での確認(カメラ撮影、実写での画面検出、USB ガジェット、
> ホットスポット)はまだ。

Raspberry Pi Zero W にカメラモジュールを載せて開発中のボードの液晶に向け、
そのボードの USB ポートにタッチパネル・キーボード・ネットワークアダプタとして
挿さる。AI エージェント(Claude Code など MCP を話すもの)が `take_screenshot`
を呼ぶとパネルに実際に描かれているものが見え、`tap` で見たものを押せる。
描画の崩れ、色、フォント、起動直後の状態など、ホスト側のスクリーンショットでは
分からないものが分かる。HDMI 出力の無い機器向けの NanoKVM、映像はカメラ、
入力は USB、と思えばよい。

```
AI エージェント ──(Tailscale / HTTP)──▶ Pi Zero W: tailvision ──(CSI)──▶ カメラモジュール ──▶ 対象の液晶
                                            │     └──(USB ケーブル 1 本)──▶ 対象機: タッチ + キーボード + 給電
スマホ / PC ──(Wi-Fi またはセットアップ用ホットスポット)──▶ 設定ページ http://tailvision-xxxx.local/
```

## 特徴

- Rust 製の静的バイナリ 1 本。Python も OpenCV も C ライブラリも要らない。
- MCP は Streamable HTTP の `/mcp`。アクセスキーは初回起動で自動生成され、
  カメラがネットワークに無防備になることはない。
- `/` の設定ページで Wi-Fi、ホスト名、Tailscale、アクセスキーを設定できる。
  ネットワークが見つからなければ自分で WPA2 ホットスポットを立てるので、
  スマホから設定できる。
- Tailscale 内蔵。どこからでも名前で届く。
- 基板の緑 LED で状態が分かる。

## MCP ツール

| Tool              | 戻り値 | 内容                                                           |
| ----------------- | ------ | -------------------------------------------------------------- |
| `take_screenshot` | image  | フレームから液晶を見つけ、透視補正して画面だけを JPEG で返す   |
| `capture_debug`   | image  | 生フレームに検出した枠を描いたもの(大きい点が左上)           |
| `tap`             | text   | 直前の `take_screenshot` 画像の画素座標で画面を触る            |
| `swipe`           | text   | 直前のスクリーンショット上の 2 点間をドラッグ                  |
| `type_text`       | text   | USB キーボードで ASCII 文字列を打つ                            |
| `calibrate_camera`| image  | フォーカス・露出・WB を 1 回測って固定(`lock=false` で解除)  |
| `press_key`       | text   | 修飾キー付きで 1 キー。`enter`、`ctrl`+`c`、`f5` など          |
| `list_formats`    | text   | UVC カメラが出せるピクセルフォーマットと解像度                 |

`take_screenshot` は毎回検出し直す(カメラは固定でなくてよい)。パラメータ:
`raw=true` で生フレーム、`reuse_detection=true` で前回の 4 隅を使い回す(固定なら速い)、
`manual_corners=[[x,y],...]` で検出を上書き、`output_width`/`output_height`、
`margin_ratio`、`rotation_degrees`(90 の倍数)、`flip_horizontal`、`flip_vertical`、
`min_area_ratio`、および撮影側の `width`/`height`、`skip_frames`、`quality`。

画像はカメラから見えたままの向きで返す。カメラはどんな角度で持ってもよく、
呼び出しの間に動いてもよい。本機は上下を推定せず、保存された向きの設定も
ない。向きの判断は画像を見るエージェントが行い、文字が横や逆さなら
`rotation_degrees` を付けて撮り直す。タップとスワイプの座標は、どの回転で
あれエージェントが受け取った画像の座標なので、ずれない。

検出は「最も液晶らしい凸四角形」を探す。候補は直線エッジ、明暗マスク、明るい
UI 要素の広がりから作り、縦横比、大きさ、表示内容をどれだけ含むか、枠に沿って
本物の明るさの段差があるか、で採点する。黒い机の上の暗いパネルでベゼルが
見えない場合が難しく、そのときは表示内容の範囲になる。`capture_debug` で確認し、
外れるときは `manual_corners` を使う。

タッチの座標はエージェントが受け取った画像の画素なので、カメラのことを
何も知らなくても「見て、決めて、押す」ができる。本機が検出した 4 隅を通して
対象機のタッチ座標に変換する(`frame_coords=true` なら `capture_debug` の
生フレームの画素でも指定できる)。`tap` の `hold_ms=800` で長押し。

カメラは 2 系統。CSI カメラモジュールは libcamera(`rpicam-still`)経由で、
露出・ホワイトバランス・AF が `--csi-settle-ms`(既定 1.5 秒)で安定するのを
待つ。UVC ウェブカメラは V4L2 経由(MJPEG はハフマンテーブルを補って
そのまま、YUYV は Pi でエンコード、最初の `skip_frames` フレームは捨てる)。
`--camera auto` は libcamera がカメラを見つければ CSI を選ぶ。

## HTTP エンドポイント

| パス          | 内容                                                        |
| ------------- | ----------------------------------------------------------- |
| `/`           | 設定ページ                                                  |
| `/screen.jpg` | 検出した画面。`?ow=800&reuse=1` と撮影パラメータ            |
| `/debug.jpg`  | 検出枠を描いた生フレーム                                    |
| `/shot.jpg`   | 生フレーム。`?w=1280&h=720&skip=10&q=85`。curl やブラウザ用 |
| `/mcp`        | MCP(Streamable HTTP)                                      |
| その他        | `/` にリダイレクト。ホットスポットでは OS の接続確認(キャプティブポータル検出)もここに来るので、つないだ時点で「ネットワークにログイン」の画面に設定ページが開く |

ポートは 80。

## 初回起動

1. イメージを焼き、カメラと電源をつないで起動する。
2. 初回起動時に本機は自分を **`tailvision-xxxx`**(xxxx は Wi-Fi MAC の
   下 4 桁。ラベルに印字する)と名付ける。これで複数台あっても mDNS 名、
   ホットスポット名、Tailscale 名が重ならない。電源投入から 1 分ほどで本機
   自身の Wi-Fi **`tailvision-xxxx-setup`**(WPA2、パスワード
   **`tailvision-setup`**。個体ごとのパスワード付きで出荷したものはラベルの値)
   が出る。ホットスポット中は LED が速い点滅。
3. スマホか PC でそれにつなぐ。インターネットに出られないネットワークに
   対してスマホや PC が出す「ネットワークにログイン」の画面に設定ページが
   開く。出なければ `http://tailvision-xxxx/`(または
   `http://tailvision-xxxx.local/`、`http://10.42.0.1/`)を開く。ホットスポットにいる相手は物理的に触れる
   相手なので、そこでは鍵は不要。
4. 設定ページで:
   - **Wi-Fi**: 一覧から選んでパスワードを入れて Join。Zero W は無線が 1 本
     なのでホットスポットは 1 分ほど消える。その間に本機はその Wi-Fi に入り、
     インターネットに出られるか確かめ、Tailscale のログイン用リンクを取って
     くる。そして**ホットスポットが戻ってきて**、ページの先頭に結果が出る:
     Wi-Fi が使えたか(駄目なら理由)、承認する Tailscale のリンク、控えておく
     アクセスキー。リンクをインターネットにつながった端末で開いて承認し、
     **Go online** を押すと本機はその Wi-Fi に切り替わり、Tailscale に入ると
     LED が点灯になる。2 分たっても入れなければホットスポットが理由付きで
     戻ってくる。
   - **Hostname**: 複数台あるときに変える。`.local` 名、Tailscale 名、
     ホットスポット名が追従する。
   - **Tailscale**: 「Get a login link」でログイン URL が出るので、どの端末からでも
     開いて承認する。auth key でも可。
   - **Camera**(CSI モジュールのみ): 「Calibrate and lock」で点灯中の画面に
     向けて AF・AE・AWB を 1 回だけ走らせ、結果を固定する。以後の撮影は全て
     同じ設定になり、毎回の収束待ちも、暗い UI での AF の迷いも無くなる。
     「Back to automatic」で戻せる。エージェントからは `calibrate_camera`。
   - **Tailscale の Log out**: ログイン済みのときに出る。本機を tailnet から
     外す(再設定や譲渡用)。以後はホットスポットからしか届かない。
   - **Access key**: ページに `claude mcp add` のコマンドごと表示される。
     「Generate a new key」で作り直せる。
5. Wi-Fi の届かない場所に持って行くと 60 秒後にまたホットスポットが立ち、
   10 分ごとに保存済み Wi-Fi を試し直す。

### LED(基板の緑 ACT LED)

| LED                   | 状態                                   |
| --------------------- | -------------------------------------- |
| 2 秒に 1 回短く光る   | ネットワーク探索中                     |
| 速い点滅(0.2 秒)    | セットアップ用ホットスポット稼働中     |
| ゆっくり点滅(1 秒)  | Wi-Fi 接続済み、Tailscale 未ログイン   |
| 点灯                  | Wi-Fi と Tailscale とも OK             |

### 誰が何をできるか

- `/mcp` と `.jpg` 各エンドポイントは常に `Authorization: Bearer <key>` が必要
  (画像は `?key=` も可)。キーは初回起動時に生成される。
- 設定ページは HTTP Basic 認証(ユーザー名は任意、パスワードがキー)。ただし
  セットアップ用ホットスポット経由の端末は例外で、ホットスポットの WPA2
  パスワードを知っている人は本機のそばにいる人として扱う。初回はこれでキーを読む。
- 個体ごとのホットスポットパスワードは、boot(FAT)パーティションの
  `hotspot-password` に書いてラベルに印字する。

## Claude Code への登録

設定ページに出るコマンドをそのまま使う。

```bash
claude mcp add --transport http tailvision http://tailvision/mcp \
  --header "Authorization: Bearer <key>"
```

`.mcp.json` なら:

```json
{
  "mcpServers": {
    "tailvision": {
      "type": "http",
      "url": "http://tailvision/mcp",
      "headers": { "Authorization": "Bearer <key>" }
    }
  }
}
```

`tailvision-xxxx` は Tailscale の MagicDNS でどこからでも、`tailvision-xxxx.local` は同じ LAN の
mDNS で引ける。

## ハードウェア

- Raspberry Pi Zero W(または Zero 2 W)。Pi OS Lite 32-bit、Trixie 以降。
- Raspberry Pi Camera Module 3(オートフォーカス、10〜20cm に向く)か
  Module 2 / OV5647(固定焦点)。Zero 用の 22 ピンカメラケーブル。
- **USB** ポートから対象機への micro-B → USB-A ケーブル。この 1 本で給電、
  タッチ、キーボードが通る。対象機の電源を切っても本機を
  生かしておきたいなら **PWR IN** に別電源を入れ、対象機側ケーブルの VBUS に
  ショットキーダイオードを入れて 5V を逆流させない。
- 8GB 以上の microSD。
- 書画カメラのようなアームで、カメラをパネルの上に保持するもの。

UVC ウェブカメラを使う場合は、OTG ケーブルで **USB** ポートに挿し、電源は
**PWR IN** から取り、`config.txt` の `dtoverlay=dwc2,dr_mode=peripheral` の行を
消す(ポートがホストになり、タッチとキーボードは使えない)。

### 対象機から見えるもの

複合 USB デバイス: シングルタッチのデジタイザ(絶対座標、両軸 0〜32767、
対象機が自分の画面に割り当てる)とブートプロトコルのキーボード。既定では
それだけで、本機は対象機のネットワークアダプタにはならず、両者の間に
ネットワーク経路はない。Linux、Android、macOS、Windows、大半の RTOS の USB
スタックでどちらもドライバ不要。

デバッグ用に CDC ECM のネットワークアダプタを足せる。起動パーティションに
`usb-ethernet` というファイルを作る(`image.env` の `USB_ETHERNET=1` がそれを
やる)か、`--usb-ethernet` を付けて起動する。本機は対象機に 10.42.1.0/24 の
アドレスを配り Wi-Fi へ NAT するので、本機から対象機へ(SSH、ADB)届く。
ファイルを消せば元に戻る。Windows には RNDIS 版が要る(未実装)。

## ビルド

Linux x86_64 から ARMv6 の musl 静的バイナリをクロスビルドする。ツールチェーン
同梱の `rust-lld` でリンクするのでクロス用 GCC は要らない。

```bash
rustup target add arm-unknown-linux-musleabihf
cargo build --release --target arm-unknown-linux-musleabihf
# → target/arm-unknown-linux-musleabihf/release/tailvision
cargo test                                   # カメラ不要
```

開発機で動かすなら
`cargo run -- --bind 127.0.0.1:8080 --state-dir ./state --no-hotspot --no-led`。

## SD カードイメージの作成

```bash
deploy/build-image.sh            # → build/tailvision.img、約 1 分
```

公式の Raspberry Pi OS Lite(32-bit, Trixie)と Tailscale の静的バイナリを
`build/` にダウンロードし、ループマウント(sudo が要る)でバイナリとサービスを
入れ、ホスト名とユーザーの cloud-init 設定を書く。Wi-Fi(Enter で飛ばして
ホットスポット任せにできる)と Tailscale の auth key(任意)を聞かれる。
それ以外は既定では何も焼き込まないので、他人に渡せるイメージになる。
LAN で sshd が動くのは、作る人が `image.env` に SSH 鍵を書いた
とき(開発用の個体)だけで、パスワードでの SSH ログインは常に無効。素の個体への
保守は tailnet の ACL で制御される Tailscale SSH と設定ページで行う。

自分の SSH 鍵や個体ごとのホットスポットパスワードは `deploy/image.env.example`
を参照。

Raspberry Pi Imager の「Use custom」で焼く(OS カスタマイズは「No」)。または:

```bash
sudo dd if=build/tailvision.img of=/dev/sdX bs=4M status=progress conv=fsync
```

初回起動は 2〜3 分かかる。

既存の Raspberry Pi OS に入れるなら:

```bash
deploy/install.sh pi@tailvision    # バイナリとサービス。Tailscale は別途
```

カードを [microsd-ota](https://github.com/signal-slot/microsd-ota) のブリッジに
挿したままなら、Pi の電源を切った状態で `deploy/update-via-bridge.sh` を使うと
バイナリとユニットだけを USB 越しに差し替えられる。イメージを作り直すたびに
ext4 の配置が変わるので差分書き込みでも数百 MB になり、こちらのほうが圧倒的に
速い。

## オプション

```
tailvision [--bind [::]:80] [--camera auto|csi|uvc] [--csi-settle-ms 1500]
           [--csi-args "--hflip"] [--no-gadget] [--device /dev/video0]
           [--width W --height H] [--skip-frames 10] [--quality 85]
           [--frame-timeout 5] [--state-dir /var/lib/tailvision]
           [--wifi-iface wlan0] [--hotspot-after 60] [--hotspot-retry 600]
           [--hotspot-password ...] [--hotspot-password-file /boot/firmware/hotspot-password]
           [--no-hotspot] [--led /sys/class/leds/ACT] [--no-led]
```

`RUST_LOG=debug` でログが増える。

## Wi-Fi に関する注意

- ホットスポットはわざと素の WPA2-PSK にしてある。NetworkManager 1.52 は AP に
  PSK-SHA256 も喋らせるが、Zero W の Wi-Fi ファームウェアはビーコンを自前で
  作り PSK しか載せないため、全クライアントがハンドシェイクを拒否する(スマホ
  は「パスワードが違う」と言う)。tailvision は AP 起動直後に WPA-PSK だけに
  固定し直す。
- Zero W は 2.4GHz のみ。5GHz 専用のネットワークは見えない。

## 液晶を撮るときのコツ

- 画角: 撮影結果に「画面がフレームの何 % か」が付く。Camera Module 3(水平
  66°)なら 7 インチで約 12cm、5 インチで約 9cm で画面いっぱい。3〜7 割
  写っていれば十分で、残りは検出が切り出す。
- フォーカス・露出・WB: 毎回の自動と格闘せず、設置時に 1 回キャリブレーション
  して固定する。シャッターをバックライト PWM の周期より長く(10ms 以上)
  すれば横縞も消える。
- モアレが出たらカメラを数度傾けるか解像度を変える。
- バックライト PWM のちらつきは露出固定で消す。例:
  `v4l2-ctl -c exposure_auto=1 -c exposure_absolute=...`
- 映り込みは、パネルとカメラを室内灯から遮る。

## 制限

- 色は校正しない。カメラが見たままを返す。
- Tailscale のログインはインターネットに出られるネットワークが要る。
- ホットスポットのパスワードは個体固定(ラベルか README の既定値)なので、
  通りすがりは防げるが、本体を手にした人は防げない。

## ライセンス

MIT. Copyright (c) 2026 Signal Slot Inc.
