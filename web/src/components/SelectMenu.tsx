// Sleek themed dropdown menu replacing native OS <select> controls across Taskmaster.
// Uses Radix Popover with opacity-only fade (zero transform animation) and roving keyboard focus.

import * as Popover from "@radix-ui/react-popover";
import { Check, ChevronDown } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import { rove } from "./roving";

export interface SelectOption<V extends string = string> {
  value: V;
  label: string;
  note?: string;
  disabled?: boolean;
}

export function SelectMenu<V extends string = string>({
  value,
  options,
  onChange,
  ariaLabel,
  title,
  icon,
  prefix,
  placeholder = "Choose",
  className = "",
  variant = "field",
  side = "bottom",
  align = "start",
  disabled = false,
  testId,
}: {
  value: V;
  options: SelectOption<V>[];
  onChange: (value: V) => void;
  ariaLabel: string;
  title?: string;
  icon?: ReactNode;
  prefix?: string;
  placeholder?: string;
  className?: string;
  /** "field" matches form inputs; "pill" fits composer/toolbar bars; "inline" fits compact headers. */
  variant?: "field" | "pill" | "inline";
  side?: "top" | "bottom";
  align?: "start" | "end" | "center";
  disabled?: boolean;
  testId?: string;
}) {
  const [open, setOpen] = useState(false);
  const current = options.find((o) => o.value === value);

  useEffect(() => {
    if (!open) return;
    const close = () => setOpen(false);
    window.addEventListener("resize", close);
    return () => window.removeEventListener("resize", close);
  }, [open]);

  const display = current?.label ?? placeholder;

  return (
    <Popover.Root open={open} onOpenChange={setOpen} modal={false}>
      <Popover.Trigger asChild>
        <button
          type="button"
          disabled={disabled}
          className={`select-trigger variant-${variant} ${className}`}
          aria-label={ariaLabel}
          aria-haspopup="listbox"
          aria-expanded={open}
          title={title}
          data-testid={testId}
          data-value={value}
        >
          {icon && <span className="select-ico" aria-hidden>{icon}</span>}
          <span className="select-label clip">
            {prefix ? <span className="select-prefix">{prefix}</span> : null}
            {display}
          </span>
          <ChevronDown size={12} className="select-chev" aria-hidden />
        </button>
      </Popover.Trigger>
      {open && (
        <Popover.Portal>
          <Popover.Content
            asChild
            side={side}
            align={align}
            sideOffset={6}
            collisionPadding={12}
            sticky="always"
            hideWhenDetached
          >
            <div className="pop select-pop" role="listbox" aria-label={ariaLabel} onKeyDown={(e) => rove(e)}>
              {options.map((opt) => {
                const selected = opt.value === value;
                return (
                  <button
                    key={opt.value}
                    type="button"
                    role="option"
                    data-rove
                    data-value={opt.value}
                    aria-selected={selected}
                    disabled={opt.disabled}
                    className={`pop-item ${selected ? "on" : ""}`}
                    onClick={() => {
                      onChange(opt.value);
                      setOpen(false);
                    }}
                  >
                    <span className="grow clip">
                      {opt.label}
                      {opt.note && <span className="pop-note"> · {opt.note}</span>}
                    </span>
                    {selected && <Check size={13} aria-hidden />}
                  </button>
                );
              })}
            </div>
          </Popover.Content>
        </Popover.Portal>
      )}
    </Popover.Root>
  );
}
