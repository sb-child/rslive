# rslive

你心心念念的 [Bevy Engine](https://bevy.org/) 已经整根插入了 [WebRTC](https://webrtc.org/) 最新最热的 [WHIP](https://www.ietf.org/archive/id/draft-ietf-wish-whip-01.html) 小穴。

## 试试水

去下载 [MediaMTX](https://github.com/bluenviron/mediamtx/releases) 然后

```shell
# 启动
./mediamtx

# 也启动
cargo r
```

浏览器打开 http://127.0.0.1:8889/mystream/ 可以看 Bevy 色情直播。

如果 ffmpeg 出问题了，你需要去[这里](./src/video_encoder.rs)改改。

## License

Mozilla Public License Version 2.0
