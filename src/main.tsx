import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./index.css";

const root = document.getElementById("root") as HTMLElement;

// 撤掉 index.html 里的首帧骨架。它是纯静态的兜底（JS 下载期间给用户看形状），
// 一旦 React 接管就必须让位，否则两套布局叠在一起。
// 用 rAF 等首帧真正画完再撤，避免"骨架消失 → 空白一帧 → 应用出现"的闪一下。
requestAnimationFrame(() => {
  document.getElementById("boot")?.remove();
  // 挂载点此前被骨架盖住，此刻才拿到尺寸，补一次首帧测量
  window.dispatchEvent(new Event("resize"));
  void root;
});

ReactDOM.createRoot(root).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
