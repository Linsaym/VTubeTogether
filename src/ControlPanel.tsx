import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState, type FormEvent, type ReactNode } from "react";

type Phase = "idle" | "starting" | "connecting" | "reconnecting" | "listening" | "connected" | "stopping" | "error";
type Status = { phase: Phase; detail: string };
type Snapshot = { sender: Status; receiver: Status; inviteCode: string; delayMs: number };
type WindowChoice = { id: string; title: string; isVtubeStudio: boolean };

const initialStatus: Status = { phase: "idle", detail: "" };
const phases: Record<Phase, string> = {
  idle: "Выключен",
  starting: "Запуск",
  connecting: "Подключение",
  reconnecting: "Переподключение",
  listening: "Ожидание",
  connected: "Подключено",
  stopping: "Остановка",
  error: "Ошибка",
};

function active(status: Status) {
  return status.phase !== "idle" && status.phase !== "error";
}

function Panel({
  title, icon, description, status, children,
}: {
  title: string; icon: string; description: string; status: Status; children: ReactNode;
}) {
  return (
    <section className="panel" aria-label={title}>
      <div className="panel-top">
        <div className="mode-icon" aria-hidden="true">{icon}</div>
        <span className={`status ${status.phase}`} role="status" title={status.detail}>
          <i aria-hidden="true" />{phases[status.phase]}
        </span>
      </div>
      <h2>{title}</h2>
      <p className="panel-description">{description}</p>
      {children}
      {status.detail && <p className={`detail ${status.phase}`}>{status.detail}</p>}
    </section>
  );
}

function Port({ id, value, onChange, disabled }: {
  id: string; value: string; onChange: (value: string) => void; disabled: boolean;
}) {
  return (
    <label htmlFor={id}>UDP-порт
      <input id={id} type="number" inputMode="numeric" min="1" max="65535" step="1"
        value={value} onChange={(event) => onChange(event.target.value)} required disabled={disabled} />
    </label>
  );
}

export function ControlPanel() {
  const [state, setState] = useState<Snapshot>({
    sender: initialStatus, receiver: initialStatus, inviteCode: "", delayMs: 0,
  });
  const [windows, setWindows] = useState<WindowChoice[]>([]);
  const [windowId, setWindowId] = useState("");
  const [host, setHost] = useState("");
  const [remotePort, setRemotePort] = useState("42042");
  const [localPort, setLocalPort] = useState("42042");
  const [code, setCode] = useState("");
  const [delay, setDelay] = useState("0");
  const [sendError, setSendError] = useState("");
  const [receiveError, setReceiveError] = useState("");
  const [sendBusy, setSendBusy] = useState(false);
  const [receiveBusy, setReceiveBusy] = useState(false);
  const [copied, setCopied] = useState(false);

  const sending = active(state.sender);
  const receiving = active(state.receiver);
  const listenerOpen = state.receiver.phase === "listening" || state.receiver.phase === "connected";

  async function refreshWindows() {
    try {
      const list = await invoke<WindowChoice[]>("list_windows");
      setWindows(list);
      setWindowId((current) => list.some(({ id }) => id === current)
        ? current : (list.find(({ isVtubeStudio }) => isVtubeStudio)?.id ?? ""));
    } catch (error) {
      setSendError(String(error));
    }
  }

  useEffect(() => {
    let live = true;
    let polling = false;
    async function poll() {
      if (polling) return;
      polling = true;
      try {
        const data = await invoke<Snapshot>("snapshot");
        if (live) setState(data);
      } catch (error) {
        if (live) setSendError(String(error));
      } finally {
        polling = false;
      }
    }
    void refreshWindows();
    void poll();
    const timer = window.setInterval(() => void poll(), 300);
    return () => { live = false; window.clearInterval(timer); };
  }, []);

  async function toggleSender(event: FormEvent) {
    event.preventDefault();
    setSendBusy(true);
    setSendError("");
    try {
      await (sending ? invoke("stop_sender") : invoke("start_sender", {
        windowId, host, port: Number(remotePort), code,
      }));
      setState(await invoke<Snapshot>("snapshot"));
    } catch (error) {
      setSendError(String(error));
    } finally {
      setSendBusy(false);
    }
  }

  async function toggleReceiver(event: FormEvent) {
    event.preventDefault();
    setReceiveBusy(true);
    setReceiveError("");
    try {
      await (receiving ? invoke("stop_receiver") : invoke("start_receiver", { port: Number(localPort) }));
      setState(await invoke<Snapshot>("snapshot"));
    } catch (error) {
      setReceiveError(String(error));
    } finally {
      setReceiveBusy(false);
    }
  }

  async function copyCode() {
    try {
      await navigator.clipboard.writeText(state.inviteCode);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1600);
    } catch {
      const input = document.getElementById("invite-code") as HTMLInputElement | null;
      input?.focus();
      input?.select();
    }
  }

  function changeDelay(value: string) {
    const digits = value.replace(/\D/g, "");
    const delayMs = Math.min(3000, Number(digits));
    setDelay(digits === "" ? "" : String(delayMs));
    void invoke("set_delay", { delayMs }).catch((error) => setReceiveError(String(error)));
  }

  return (
    <main className="shell">
      <header className="app-header">
        <div className="brand-icon" aria-hidden="true">⇄</div>
        <div><h1>VTubeTogether</h1><p>Прямая передача модели</p></div>
      </header>
      <div className="workspace">
        <Panel title="Отправитель" icon="↗" description="Передавайте окно VTube Studio собеседнику." status={state.sender}>
          <form onSubmit={toggleSender} className="mode-form">
            <div>
              <label htmlFor="source">Окно захвата</label>
              <div className="field-with-action">
                <select id="source" value={windowId} required disabled={sending}
                  onChange={(event) => setWindowId(event.target.value)}>
                  <option value="">Выберите окно</option>
                  {windows.map((item) => <option key={item.id} value={item.id}>{item.title}</option>)}
                </select>
                <button type="button" className="icon-button" aria-label="Обновить список окон" title="Обновить список окон"
                  disabled={sending} onClick={() => void refreshWindows()}>↻</button>
              </div>
            </div>
            <div className="field-row">
              <label htmlFor="remote-ip">IP приёмника
                <input id="remote-ip" placeholder="192.168.1.20" value={host} required disabled={sending}
                  spellCheck={false} onChange={(event) => setHost(event.target.value)} />
              </label>
              <Port id="remote-port" value={remotePort} onChange={setRemotePort} disabled={sending} />
            </div>
            <label htmlFor="remote-code">Код подключения
              <input id="remote-code" type="password" placeholder="Вставьте код приёмника" value={code}
                autoComplete="off" spellCheck={false} onChange={(event) => setCode(event.target.value)}
                required disabled={sending} />
            </label>
            <button className={sending ? "secondary push" : "primary push"} disabled={sendBusy || state.sender.phase === "stopping"}>
              {sending ? "Остановить отправку" : "Подключить отправитель"}
            </button>
          </form>
          {sendError && <p className="detail error" role="alert">{sendError}</p>}
        </Panel>
        <Panel title="Приёмник" icon="↙" description="Принимайте поток в отдельное окно OBS." status={state.receiver}>
          <form className="mode-form" onSubmit={toggleReceiver}>
            <div className="field-row receiver-row">
              <Port id="local-port" value={localPort} onChange={setLocalPort} disabled={receiving} />
              <label htmlFor="delay">Задержка видео
                <span className="unit-input">
                  <input id="delay" inputMode="numeric" value={delay}
                    onChange={(event) => changeDelay(event.target.value)}
                    onBlur={() => setDelay(delay || "0")} />
                  <span aria-hidden="true">мс</span>
                </span>
              </label>
            </div>
            <div>
              <label htmlFor="invite-code">Код подключения</label>
              <div className="field-with-action">
                <input id="invite-code" value={listenerOpen ? state.inviteCode : ""} readOnly
                  placeholder="Появится после запуска" spellCheck={false} />
                <button className="secondary compact" type="button" onClick={() => void copyCode()} disabled={!listenerOpen}>
                  {copied ? "Скопировано" : "Копировать"}
                </button>
              </div>
            </div>
            <div className="receiver-actions push">
              <button className={receiving ? "secondary" : "primary"} disabled={receiveBusy || state.receiver.phase === "stopping"}>
                {receiving ? "Остановить приём" : "Запустить приёмник"}
              </button>
              <button className="secondary" type="button"
                onClick={() => void invoke("show_video").catch((error) => setReceiveError(String(error)))}>
                Окно для OBS ↗
              </button>
            </div>
          </form>
          {receiveError && <p className="detail error" role="alert">{receiveError}</p>}
        </Panel>
      </div>
      <footer className="app-footer">
        <span><i aria-hidden="true" />Отправка и приём работают одновременно</span>
        <span>H.264 · QUIC · до 30 FPS</span>
      </footer>
    </main>
  );
}
