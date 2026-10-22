import { Button } from "@driif/motiv";
import { listen } from "@tauri-apps/api/event";
import { RotateCw, TriangleAlert } from "lucide-react";
import { Component, ReactNode, useEffect, useState } from "react";
import { BACKEND_LOST } from "./api";
import { errorMessage } from "./errorMessage";

/** The reload screen. The shields keep running in the backend. */
export function ReloadScreen({ title, detail }: { title: string; detail?: string }) {
  return (
    <div className="empty">
      <TriangleAlert size={40} className="warn" />
      <h1>{title}</h1>
      <p className="muted">Your shields keep running. Reload the window to continue.</p>
      {detail && <p className="muted mono">{detail}</p>}
      <Button iconLeft={<RotateCw size={16} />} onClick={() => location.reload()}>
        Reload
      </Button>
    </div>
  );
}

export const LOST_TITLE = "zwinger lost the connection to its backend";

/** True after the backend panicked, or after repeated failed commands. */
export function useBackendLost() {
  const [lost, setLost] = useState(false);
  useEffect(() => {
    const onLost = () => setLost(true);
    window.addEventListener(BACKEND_LOST, onLost);
    const stop = listen(BACKEND_LOST, onLost);
    return () => {
      window.removeEventListener(BACKEND_LOST, onLost);
      stop.then((unlisten) => unlisten());
    };
  }, []);
  return lost;
}

/** A render error shows a reload screen instead of a white window. */
export class Recover extends Component<{ children: ReactNode }, { error?: unknown }> {
  state: { error?: unknown } = {};

  static getDerivedStateFromError(error: unknown) {
    return { error };
  }

  render() {
    if (this.state.error === undefined) return this.props.children;
    return <ReloadScreen title="zwinger stopped showing this window" detail={errorMessage(this.state.error)} />;
  }
}
