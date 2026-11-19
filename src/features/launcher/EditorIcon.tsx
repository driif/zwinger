import { Code } from "lucide-react";
import { useState } from "react";

/** The app icon of an editor, else the generic editor icon. Decorative: the label next to it names the editor. */
export function EditorIcon({ icon }: { icon: string | null }) {
  const [failed, setFailed] = useState<string | null>(null);
  if (!icon || failed === icon) return <Code size={14} />;
  return <img src={icon} alt="" width={14} height={14} onError={() => setFailed(icon)} />;
}
