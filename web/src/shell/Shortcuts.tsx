// The keyboard sheet, on ?. Every shortcut the app answers to, in one place.

import * as Dialog from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import { MOD } from "../desktop";

export const SHORTCUTS: { group: string; items: [string, string[]][] }[] = [
  {
    group: "Anywhere",
    items: [
      ["Search projects, runs, files and commands", [MOD, "K"]],
      ["Commands and skills", ["/"]],
      ["Start a new run", ["N"]],
      ["Show or hide the terminal", [MOD, "J"]],
      ["Open the files panel", [MOD, "⇧", "E"]],
      ["Settings", [MOD, ","]],
      ["This sheet", ["?"]],
      ["Undo the last change", [MOD, "Z"]],
    ],
  },
  {
    group: "Runs",
    items: [
      ["Open run 1 to 9 in the sidebar", [MOD, "1–9"]],
      ["Previous or next run", ["Alt", "↑ ↓"]],
      ["Show or hide the side panel", [MOD, "\\"]],
      ["Move between agents", ["J", "K"]],
      ["Expand the focused agent", ["Enter"]],
      ["Open the focused file", ["E"]],
      ["Close the side panel", ["Esc"]],
    ],
  },
  {
    group: "Files",
    items: [
      ["Find in the open file", [MOD, "F"]],
      ["Go to line", [MOD, "G"]],
      ["Search every file", [MOD, "⇧", "F"]],
      ["Show the run's change to the file", ["D"]],
    ],
  },
  {
    group: "Terminal",
    items: [
      ["Hide it (a full-screen program keeps Esc)", ["Esc"]],
      ["Resize from the top edge", ["↑ ↓"]],
    ],
  },
];

export function Shortcuts({ open, onOpenChange }: { open: boolean; onOpenChange: (open: boolean) => void }) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="dlg-back" />
        <Dialog.Content className="dlg keys-sheet" aria-describedby={undefined}>
          <div className="dlg-head">
            <Dialog.Title className="dlg-title">Keyboard</Dialog.Title>
            <Dialog.Close asChild>
              <button type="button" className="icon" aria-label="Close">
                <X size={15} />
              </button>
            </Dialog.Close>
          </div>
          <div className="keys-grid">
            {SHORTCUTS.map((g) => (
              <section key={g.group}>
                <h3 className="keys-group">{g.group}</h3>
                <dl>
                  {g.items.map(([label, combo]) => (
                    <div key={label} className="keys-row">
                      <dt>{label}</dt>
                      <dd>
                        {combo.map((k) => (
                          <kbd key={k}>{k}</kbd>
                        ))}
                      </dd>
                    </div>
                  ))}
                </dl>
              </section>
            ))}
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
