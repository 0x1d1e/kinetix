import React, { useEffect, useId, useRef } from 'react';
import { X } from 'lucide-react';

interface ModalProps {
  open: boolean;
  title: string;
  onClose: () => void;
  children: React.ReactNode;
  className?: string;
}

/** Native modal dialog with browser-managed focus, Escape handling, and focus return. */
export const Modal: React.FC<ModalProps> = ({ open, title, onClose, children, className = '' }) => {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const titleRef = useRef<HTMLHeadingElement>(null);
  const titleId = useId();

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    if (open && !dialog.open) {
      dialog.showModal();
      titleRef.current?.focus();
    }
    if (!open && dialog.open) dialog.close();
  }, [open]);

  return (
    <dialog
      ref={dialogRef}
      aria-labelledby={titleId}
      onCancel={(event) => {
        event.preventDefault();
        onClose();
      }}
      className={`kinetix-modal max-h-[90dvh] w-[calc(100%-2rem)] max-w-3xl overflow-y-auto border-2 border-[var(--ink)] bg-[var(--paper)] p-0 text-[var(--ink)] sketch-shadow ${className}`}
    >
      <div className="flex items-center justify-between gap-4 border-b-2 border-dashed border-[var(--ink)]/20 px-5 py-3">
        <h2 ref={titleRef} id={titleId} tabIndex={-1} className="text-xl font-heading font-bold">{title}</h2>
        <button
          type="button"
          onClick={onClose}
          className="shrink-0 border-2 border-[var(--ink)] bg-[var(--surface)] p-2 cursor-pointer hover:bg-[var(--erased)] focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--pen-blue)]"
          aria-label={`Close ${title}`}
        >
          <X className="w-4 h-4" aria-hidden="true" />
        </button>
      </div>
      {children}
    </dialog>
  );
};
