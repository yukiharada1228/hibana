// Portions derived from or inspired by digital-go-jp/design-system-example-components-react.
// Original code licensed under the MIT License.
// See THIRD_PARTY_LICENSES.md for details.
import * as React from "react";

import { cn } from "@/lib/digital-agency/cn";

const Dialog = React.forwardRef<
  HTMLDialogElement,
  React.ComponentProps<"dialog">
>(({ children, className, ...props }, ref) => (
  <dialog
    ref={ref}
    data-slot="dialog"
    className={cn(
      "inset-0 w-auto h-auto max-w-none max-h-none border-0 bg-transparent px-4 [container-type:inline-size] [color-scheme:dark] break-words text-std-16N-170 [&:modal]:flex [&:modal]:flex-col [&:modal]:items-center backdrop:bg-opacity-gray-600 forced-colors:backdrop:bg-[#000b] [scrollbar-gutter:stable]",
      className,
    )}
    {...props}
  >
    <div className="shrink-[9999] w-px h-[calc(120/16*1rem)] min-h-4" />
    {children}
    <div className="shrink-[9999] w-px h-[calc(120/16*1rem)] min-h-4" />
  </dialog>
));
Dialog.displayName = "Dialog";

const DialogContent = React.forwardRef<
  HTMLDivElement,
  React.ComponentProps<"div">
>(({ className, ...props }, ref) => (
  <div
    ref={ref}
    data-slot="dialog-content"
    className={cn(
      "flex flex-col gap-y-3 shrink-0 w-fit min-w-[min(30rem,calc(100cqw-2rem))] max-w-full min-h-0 rounded-8 border border-black bg-white shadow-3 text-solid-gray-800 [color-scheme:light] md:gap-y-4",
      className,
    )}
    {...props}
  />
));
DialogContent.displayName = "DialogContent";

const DialogHeader = React.forwardRef<
  HTMLDivElement,
  React.ComponentProps<"div">
>(({ className, ...props }, ref) => (
  <div
    ref={ref}
    data-slot="dialog-header"
    className={cn(
      "flex items-start shrink-0 gap-x-4 min-w-0 pt-2 px-4 md:pt-6 md:px-6",
      className,
    )}
    {...props}
  />
));
DialogHeader.displayName = "DialogHeader";

const DialogBody = React.forwardRef<
  HTMLDivElement,
  React.ComponentProps<"div">
>(({ className, ...props }, ref) => (
  <div
    ref={ref}
    data-slot="dialog-body"
    className={cn("shrink-0 min-w-0 px-4 pb-8 md:px-6", className)}
    {...props}
  />
));
DialogBody.displayName = "DialogBody";

const DialogActions = React.forwardRef<
  HTMLDivElement,
  React.ComponentProps<"div">
>(({ className, ...props }, ref) => (
  <div
    ref={ref}
    data-slot="dialog-actions"
    className={cn("shrink-0 min-w-0 px-4 pb-4 md:px-6 md:pb-6", className)}
    {...props}
  />
));
DialogActions.displayName = "DialogActions";

export { Dialog, DialogContent, DialogHeader, DialogBody, DialogActions };
