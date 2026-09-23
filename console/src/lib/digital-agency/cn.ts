import { clsx, type ClassValue } from "clsx";
import { extendTailwindMerge } from "tailwind-merge";

// Register the Digital Agency scales through Tailwind v4's theme namespaces.
// The library handles color/size separation and every directional utility.
const twMerge = extendTailwindMerge({
  extend: {
    theme: {
      text: [(value: string) => /^(dsp|std|dns|oln|mono)-\S+$/.test(value)],
      radius: ["4", "6", "8", "12", "16", "24", "32"],
      shadow: ["1", "2", "3", "4", "5", "6", "7", "8"],
      leading: [(value: string) => /^(\d{2,3}|1-\d{1,2})$/.test(value)],
    },
  },
});

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}
