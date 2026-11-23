import { createRoot } from "react-dom/client";
import { ControlPanel } from "./ControlPanel";
import { VideoWindow } from "./VideoWindow";
import "./style.css";

const root = document.getElementById("root");
if (!root) throw new Error("Missing app root");

createRoot(root).render(
  new URLSearchParams(location.search).has("video") ? <VideoWindow /> : <ControlPanel />,
);
