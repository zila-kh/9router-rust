"use client";

import { useState } from "react";
import BasicChatPageClient from "../basic-chat/BasicChatPageClient";
import BenchmarkPanel from "./BenchmarkPanel";

export default function PlaygroundPage() {
  const [tab, setTab] = useState("chat");

  return (
    <div className="flex h-full min-h-0 flex-col bg-[#212121] text-white">
      <div className="flex shrink-0 items-center justify-between border-b border-white/10 px-4 py-2 lg:px-6">
        <div>
          <h1 className="text-base font-semibold">Playground</h1>
          <p className="text-xs text-white/45">Chat with routed targets or measure native-wire overhead.</p>
        </div>
        <div className="flex rounded-xl border border-white/10 bg-black/20 p-1">
          {[
            ["chat", "Chat", "chat"],
            ["benchmark", "Benchmark", "speed"],
          ].map(([id, label, icon]) => (
            <button
              key={id}
              type="button"
              onClick={() => setTab(id)}
              className={`flex items-center gap-2 rounded-lg px-3 py-1.5 text-xs font-medium transition ${tab === id ? "bg-white text-black" : "text-white/60 hover:text-white"}`}
            >
              <span className="material-symbols-outlined text-[16px]">{icon}</span>
              {label}
            </button>
          ))}
        </div>
      </div>
      <div className="min-h-0 flex-1">
        {tab === "chat" ? <BasicChatPageClient /> : <BenchmarkPanel />}
      </div>
    </div>
  );
}
