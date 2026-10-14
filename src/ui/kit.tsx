// Small shared primitives.
import type { ButtonHTMLAttributes, ReactNode } from "react";

export function Button({
  variant = "secondary",
  size = "md",
  className = "",
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: "primary" | "secondary" | "ghost" | "danger"; size?: "sm" | "md" }) {
  const base =
    "inline-flex shrink-0 items-center justify-center gap-1.5 whitespace-nowrap rounded-lg font-medium transition-[background,opacity,transform] active:scale-[0.97] disabled:opacity-45 disabled:active:scale-100 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-accent)]";
  const sizes = size === "sm" ? "h-7 px-2.5 text-[12px]" : "h-8 px-3.5 text-[13px]";
  const variants = {
    primary: "bg-[var(--color-accent)] text-white hover:bg-[var(--color-accent-strong)] shadow-sm",
    secondary: "bg-[var(--color-chip)] text-[var(--color-ink)] hover:bg-black/10 dark:hover:bg-white/12",
    ghost: "text-[var(--color-ink-2)] hover:bg-[var(--color-chip)]",
    danger: "bg-[var(--color-bad)]/10 text-[var(--color-bad)] hover:bg-[var(--color-bad)]/18",
  }[variant];
  return <button className={`${base} ${sizes} ${variants} ${className}`} {...props} />;
}

export function IconButton({
  label,
  className = "",
  children,
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & { label: string; children: ReactNode }) {
  return (
    <button
      aria-label={label}
      title={label}
      className={`inline-flex h-7 w-7 items-center justify-center rounded-md text-[var(--color-ink-2)] transition hover:bg-[var(--color-chip)] hover:text-[var(--color-ink)] active:scale-95 disabled:opacity-40 ${className}`}
      {...props}
    >
      {children}
    </button>
  );
}

export function Spinner({ size = 14 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" className="animate-spin text-[var(--color-ink-3)]" aria-hidden>
      <circle cx="12" cy="12" r="9" fill="none" stroke="currentColor" strokeOpacity="0.25" strokeWidth="3" />
      <path d="M21 12a9 9 0 0 0-9-9" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" />
    </svg>
  );
}

/** Three bouncing dots — "the guy is typing". */
export function TypingDots({ className = "" }: { className?: string }) {
  return (
    <span className={`inline-flex items-center gap-[3px] ${className}`} aria-label="working">
      {[0, 1, 2].map((i) => (
        <span
          key={i}
          className="h-[5px] w-[5px] rounded-full bg-current opacity-60"
          style={{ animation: `lg-bounce 1.1s ${i * 0.15}s infinite ease-in-out` }}
        />
      ))}
      <style>{`@keyframes lg-bounce{0%,80%,100%{transform:translateY(0);opacity:.35}40%{transform:translateY(-3px);opacity:.9}}`}</style>
    </span>
  );
}

export function Segmented<T extends string>({
  value,
  options,
  onChange,
}: {
  value: T;
  options: Array<{ value: T; label: ReactNode }>;
  onChange: (v: T) => void;
}) {
  return (
    <div className="flex rounded-lg bg-[var(--color-chip)] p-0.5">
      {options.map((o) => (
        <button
          key={o.value}
          onClick={() => onChange(o.value)}
          className={`flex-1 rounded-md px-2.5 py-1 text-[12px] font-medium transition ${
            value === o.value
              ? "bg-[var(--color-pane-solid)] text-[var(--color-ink)] shadow-sm"
              : "text-[var(--color-ink-2)] hover:text-[var(--color-ink)]"
          }`}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

/** An on/off switch; `label` is its tooltip. */
export function Switch({
  on,
  onChange,
  label,
  disabled,
  className = "",
}: {
  on: boolean;
  onChange: (on: boolean) => void;
  label?: string;
  disabled?: boolean;
  className?: string;
}) {
  return (
    <button
      role="switch"
      aria-checked={on}
      disabled={disabled}
      onClick={() => onChange(!on)}
      title={label}
      className={`relative h-[18px] w-[30px] shrink-0 rounded-full transition ${on ? "bg-[var(--color-ok)]" : "bg-[var(--color-ink-3)]/35"} disabled:opacity-50 ${className}`}
    >
      <span className={`absolute top-[2px] h-[14px] w-[14px] rounded-full bg-white shadow transition-all ${on ? "left-[14px]" : "left-[2px]"}`} />
    </button>
  );
}

export function Field({ label, hint, children }: { label: string; hint?: ReactNode; children: ReactNode }) {
  return (
    <label className="block">
      <span className="mb-1 block text-[11px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">{label}</span>
      {children}
      {hint && <span className="mt-1 block text-[11.5px] text-[var(--color-ink-3)]">{hint}</span>}
    </label>
  );
}

/** Input styling without a width (pair with w-20, w-auto…). */
export const inputBase =
  "rounded-lg border border-[var(--color-line)] bg-[var(--color-pane-solid)] px-2.5 py-1.5 text-[13px] text-[var(--color-ink)] outline-none transition placeholder:text-[var(--color-ink-3)] focus:border-[var(--color-accent)] focus:ring-2 focus:ring-[var(--color-accent)]/25 selectable";

export const inputClass = `${inputBase} w-full`;

export function Modal({ children, onClose, width = 480 }: { children: ReactNode; onClose: () => void; width?: number }) {
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/25 p-6 backdrop-blur-[2px] dark:bg-black/45"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="animate-pop max-h-full overflow-y-auto rounded-2xl border border-[var(--color-line)] bg-[var(--color-pane-solid)] shadow-2xl"
        style={{ width }}
        onKeyDown={(e) => {
          if (e.key === "Escape") onClose();
        }}
      >
        {children}
      </div>
    </div>
  );
}
