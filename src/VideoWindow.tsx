import { invoke } from "@tauri-apps/api/core";
import { useEffect, useRef, useState } from "react";

function annexBCodec(data: Uint8Array) {
  for (let i = 0; i + 6 < data.length; i++) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1 && (data[i + 3]! & 31) === 7) {
      const profile = Array.from(data.subarray(i + 4, i + 7), (n) => n.toString(16).padStart(2, "0")).join("");
      return `avc1.${profile}`;
    }
  }
}

export function VideoWindow() {
  const surface = useRef<HTMLCanvasElement>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    if (typeof VideoDecoder === "undefined") {
      setError("Обновите Microsoft Edge WebView2 для воспроизведения H.264.");
      return;
    }
    const canvas = surface.current;
    const context = canvas?.getContext("2d", { alpha: false });
    if (!canvas || !context) return;

    let live = true;
    let epoch = 0xffffffff;
    let timestamp = 0;
    let decoder: VideoDecoder | undefined;
    let currentCodec = "";

    function closeDecoder() {
      if (decoder && decoder.state !== "closed") decoder.close();
      decoder = undefined;
      currentCodec = "";
    }

    function newDecoder(codec: string) {
      closeDecoder();
      const next = new VideoDecoder({
        output(frame) {
          try {
            if (!live || decoder !== next) return;
            if (canvas!.width !== frame.displayWidth || canvas!.height !== frame.displayHeight) {
              canvas!.width = frame.displayWidth;
              canvas!.height = frame.displayHeight;
            }
            context!.drawImage(frame, 0, 0);
            setError("");
          } finally {
            frame.close();
          }
        },
        error() {
          if (decoder !== next || !live) return;
          setError("Не удалось декодировать H.264. Ожидание следующего ключевого кадра.");
          closeDecoder();
        },
      });
      decoder = next;
      next.configure({ codec, optimizeForLatency: true });
      currentCodec = codec;
    }

    async function read() {
      while (live) {
        let packet: Uint8Array;
        try {
          packet = new Uint8Array(await invoke<ArrayBuffer>("viewer_read", { epoch }));
        } catch (failure) {
          if (live) setError(String(failure));
          return;
        }
        if (!live || packet.byteLength < 4) continue;
        const packetEpoch = new DataView(packet.buffer).getUint32(0);
        if (packetEpoch !== epoch) {
          epoch = packetEpoch;
          timestamp = 0;
          closeDecoder();
          context!.clearRect(0, 0, canvas!.width, canvas!.height);
          setError("");
        }
        if (packet.byteLength <= 5) continue;
        const key = packet[4] === 1;
        const data = packet.subarray(5);
        if (decoder && decoder.decodeQueueSize > 6) closeDecoder();
        try {
          if (key) {
            const codec = annexBCodec(data);
            if (codec && currentCodec !== codec) newDecoder(codec);
          }
          decoder?.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp, data }));
        } catch {
          closeDecoder();
          setError("Не удалось воспроизвести H.264. Ожидание следующего ключевого кадра.");
        }
        timestamp += 33333;
      }
    }

    void read();
    return () => { live = false; closeDecoder(); };
  }, []);

  return (
    <main className="video-window">
      <canvas ref={surface} aria-label="Полученное видео" />
      {error && <p className="video-error" role="alert">{error}</p>}
    </main>
  );
}
