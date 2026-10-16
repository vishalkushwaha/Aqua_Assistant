import { useEffect } from "react";
import { CheckCircle2, Brain, Mic, Shield, HardDrive, Volume2, RefreshCw } from "lucide-react";

export interface ServiceStatus {
  llm_gguf: boolean;
  whisper_ggml: boolean;
  kokoro_onnx: boolean;
  deepfilternet_onnx: boolean;
  biometrics_onnx: boolean;
}

interface Item {
  key: keyof ServiceStatus;
  label: string;
  required: boolean;
  icon: typeof Brain;
  how: string;
}

// Core system service dependencies required locally.
const ITEMS: Item[] = [
  { key: "llm_gguf", label: "Core Assistant Intelligence", required: true, icon: Brain, how: "Contact administrator or run system setup" },
  { key: "whisper_ggml", label: "Voice Recognition Service", required: true, icon: Mic, how: "Contact administrator or run system setup" },
  { key: "kokoro_onnx", label: "Speech Synthesis Service", required: true, icon: Volume2, how: "Contact administrator or run system setup" },
  { key: "deepfilternet_onnx", label: "Audio Clarification Filter (optional)", required: false, icon: Shield, how: "Optional audio filter component" },
  { key: "biometrics_onnx", label: "Voice Security Service (optional)", required: false, icon: Shield, how: "Optional security component" },
];

/**
 * First-run check for required assistant services.
 */
export function OnboardingWizard({
  services,
  connected,
  onComplete,
}: {
  services: ServiceStatus | null;
  connected: boolean;
  onComplete: () => void;
}) {
  const requiredReady = services !== null && ITEMS.filter((i) => i.required).every((i) => services[i.key]);

  useEffect(() => {
    if (requiredReady) onComplete();
  }, [requiredReady, onComplete]);

  if (requiredReady) return null;

  return (
    <div
      role="dialog"
      aria-label="First-run setup"
      style={{
        position: "fixed", inset: 0, zIndex: 9999, padding: 20,
        background: "radial-gradient(circle at center, rgba(16, 25, 36, 1) 0%, rgba(5, 9, 15, 1) 100%)",
        display: "flex", alignItems: "center", justifyContent: "center",
      }}
    >
      <div
        style={{
          width: "100%", maxWidth: 680, padding: 36, borderRadius: 24, textAlign: "left",
          background: "rgba(255,255,255,0.02)", border: "1px solid rgba(255,255,255,0.08)",
          boxShadow: "0 30px 60px rgba(0,0,0,0.5)", display: "flex", flexDirection: "column", gap: 16,
        }}
      >
        <h1 style={{ fontSize: 26, fontWeight: 700, color: "white", margin: 0 }}>Welcome to Aqua</h1>
        <p style={{ fontSize: 14, color: "var(--text-secondary)", lineHeight: 1.6, margin: 0 }}>
          Aqua runs locally on your computer with complete privacy. Initializing required system services.
        </p>

        {!connected || services === null ? (
          <p style={{ color: "#fbbf24", fontSize: 14, margin: 0 }}>Connecting to assistant services…</p>
        ) : (
          <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
            {ITEMS.map((i) => {
              const present = services[i.key];
              const Icon = i.icon;
              return (
                <div key={i.key} style={{ padding: 12, background: "rgba(0,0,0,0.2)", borderRadius: 12 }}>
                  <div style={{ display: "flex", alignItems: "center", gap: 12 }}>
                    <Icon size={20} color={present ? "#10b981" : "var(--text-muted)"} />
                    <span style={{ color: "white", fontSize: 14, flex: 1 }}>{i.label}</span>
                    {present ? <CheckCircle2 size={18} color="#10b981" /> : <HardDrive size={18} color="var(--text-muted)" />}
                  </div>
                  {!present && (
                    <div style={{ marginTop: 6, marginLeft: 32, fontSize: 12, color: "var(--text-muted)" }}>
                      Status: {i.how}.
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}

        <p style={{ fontSize: 12, color: "var(--text-muted)", margin: 0 }}>
          System services are automatically verified before proceeding.
        </p>

        <div style={{ display: "flex", gap: 12, alignItems: "center" }}>
          <span style={{ display: "flex", alignItems: "center", gap: 6, fontSize: 12, color: "var(--text-muted)" }}>
            <RefreshCw size={14} /> Checking…
          </span>
          <button
            onClick={onComplete}
            style={{
              marginLeft: "auto", background: "transparent", color: "var(--text-secondary)", cursor: "pointer",
              border: "1px solid rgba(255,255,255,0.2)", borderRadius: 999, padding: "10px 22px", fontSize: 13,
            }}
          >
            Continue without them
          </button>
        </div>
      </div>
    </div>
  );
}
