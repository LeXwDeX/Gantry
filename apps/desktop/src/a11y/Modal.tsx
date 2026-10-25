import type { ReactNode, RefObject } from "react";
import { useEffect, useRef } from "react";

export type ModalVariant = "dialog" | "fullscreen" | "drawer";

export interface ModalProps {
  open: boolean;
  label: string;
  onClose: () => void;
  variant?: ModalVariant;
  className?: string;
  children: ReactNode;
  initialFocusRef?: RefObject<HTMLElement | null>;
}

/**
 * 共享的原生 `<dialog>`：原生焦点限制 + Esc 取消 + 关闭后焦点恢复。
 * 全屏与抽屉只改变 className，不改变语义。
 */
export function Modal({
  open,
  label,
  onClose,
  variant = "dialog",
  className,
  children,
  initialFocusRef,
}: ModalProps): ReactNode {
  const dialogRef = useRef<HTMLDialogElement>(null);

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    if (open && !dialog.open) {
      dialog.showModal();
      const target =
        initialFocusRef?.current ??
        dialog.querySelector<HTMLElement>("[autofocus]") ??
        dialog;
      target.focus();
    } else if (!open && dialog.open) {
      dialog.close();
    }
  }, [open, initialFocusRef]);

  useEffect(() => {
    if (!open) return;
    const previous =
      document.activeElement instanceof HTMLElement
        ? document.activeElement
        : null;
    return () => {
      if (previous?.isConnected) previous.focus();
    };
  }, [open]);

  const variantClass =
    variant === "fullscreen"
      ? "dialog--fullscreen"
      : variant === "drawer"
        ? "dialog--drawer"
        : "";
  const classes = ["dialog", variantClass, className].filter(Boolean).join(" ");

  return (
    <dialog
      ref={dialogRef}
      className={classes}
      aria-label={label}
      aria-modal="true"
      onCancel={(event) => {
        event.preventDefault();
        if (open) onClose();
      }}
      onClose={() => {
        if (open) onClose();
      }}
    >
      <div className="dialog__surface">{children}</div>
    </dialog>
  );
}
