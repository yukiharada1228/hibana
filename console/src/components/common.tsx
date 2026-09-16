import { useEffect, useRef, type ReactNode } from "react";
import { Button } from "./ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogBody,
  DialogActions,
} from "./ui/dialog";

export function Icon({
  name,
}: {
  name: "apps" | "usage" | "deploy" | "refresh";
}) {
  const paths = {
    apps: "M3 3h7v7H3z M14 3h7v7h-7z M3 14h7v7H3z M14 14h7v7h-7z",
    usage: "M4 20h16 M7 16V9 M12 16V4 M17 16v-5",
    deploy: "M12 16V3 M7 8l5-5 5 5 M4 15v6h16v-6",
    refresh:
      "M20 7v5h-5 M4 17v-5h5 M5 8a8 8 0 0 1 13-3l2 3 M4 16l2 3a8 8 0 0 0 13-3",
  };
  return (
    <svg
      viewBox="0 0 24 24"
      width="20"
      height="20"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.6"
      aria-hidden="true"
    >
      <path d={paths[name]} />
    </svg>
  );
}
export function Brand() {
  return (
    <span className="brand">
      <svg width="27" height="32" viewBox="0 0 27 32" aria-hidden="true">
        <path fill="currentColor" d="M16 0 2 19h10L9 32l16-20H14z" />
      </svg>
      hibana<span className="brand-caption">CONSOLE</span>
    </span>
  );
}
export function Notice({
  children,
  error = false,
}: {
  children: ReactNode;
  error?: boolean;
}) {
  return (
    <div
      className={error ? "notice error" : "notice"}
      role={error ? "alert" : "status"}
    >
      {children}
    </div>
  );
}
export function Badge({
  active,
  children,
}: {
  active?: boolean;
  children: ReactNode;
}) {
  return (
    <span className={`badge ${active ? "active" : ""}`}>
      <span aria-hidden="true" />
      {children}
    </span>
  );
}
export function Empty({
  title,
  children,
}: {
  title: string;
  children: ReactNode;
}) {
  return (
    <div className="empty">
      <Icon name="apps" />
      <h2>{title}</h2>
      <p>{children}</p>
    </div>
  );
}
export const date = (value: string) =>
  new Intl.DateTimeFormat("ja-JP", {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(value));
// Keep user-supplied versions intact; shorten only the CLI's automatic format.
export function versionLabel(version: string) {
  const automatic = /^0\.0\.0-dev\.\d+\.([a-f0-9]{8})$/.exec(version);
  return automatic ? `自動 ${automatic[1]}` : version;
}
export const number = (value: number) =>
  new Intl.NumberFormat("ja-JP").format(value);
export const bytes = (value: number) =>
  value >= 1048576
    ? `${(value / 1048576).toFixed(1)} MiB`
    : `${(value / 1024).toFixed(1)} KiB`;

export function Confirm({
  title,
  children,
  busy,
  confirmLabel,
  confirmDisabled = false,
  onCancel,
  onConfirm,
}: {
  title: string;
  children: ReactNode;
  busy: boolean;
  confirmLabel: string;
  confirmDisabled?: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const dialog = ref.current!;
    const before = document.activeElement as HTMLElement | null;
    dialog.showModal();
    return () => {
      dialog.close();
      before?.focus();
    };
  }, []);
  return (
    <Dialog
      ref={ref}
      aria-labelledby="confirm-title"
      onCancel={(event) => {
        event.preventDefault();
        if (!busy) onCancel();
      }}
    >
      <DialogContent>
        <DialogHeader>
          <h2 id="confirm-title">{title}</h2>
        </DialogHeader>
        <DialogBody>{children}</DialogBody>
        <DialogActions>
          <Button variant="outline" disabled={busy} onClick={onCancel}>
            キャンセル
          </Button>
          <Button disabled={busy || confirmDisabled} onClick={onConfirm}>
            {busy ? "処理中…" : confirmLabel}
          </Button>
        </DialogActions>
      </DialogContent>
    </Dialog>
  );
}
