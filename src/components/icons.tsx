/** Minimal inline SVG icon components (Lucide-style) */
type IconProps = { class?: string; size?: number };
const S = (props: IconProps) => ({ width: props.size ?? 16, height: props.size ?? 16, class: props.class ?? "h-4 w-4", viewBox: "0 0 24 24", fill: "none", stroke: "currentColor", "stroke-width": "2", "stroke-linecap": "round" as const, "stroke-linejoin": "round" as const });

// Material Symbols icon component
type MaterialIconProps = { name: string; class?: string; size?: "sm" | "md" | "lg" | "xl" };
export function MaterialIcon(props: MaterialIconProps) {
  const sizeMap = { sm: "text-[16px]", md: "text-[20px]", lg: "text-[24px]", xl: "text-[28px]" };
  return (
    <span class={`material-symbols-outlined ${sizeMap[props.size ?? "md"]} ${props.class ?? ""}`}>
      {props.name}
    </span>
  );
}

// SVG Icons (fallbacks and additional icons)
export const PlusIcon         = (p: IconProps) => <svg {...S(p)}><line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/></svg>;
export const ChevronDownIcon  = (p: IconProps) => <svg {...S(p)}><polyline points="6 9 12 15 18 9"/></svg>;
export const ChevronRightIcon = (p: IconProps) => <svg {...S(p)}><polyline points="9 18 15 12 9 6"/></svg>;
export const Trash2Icon       = (p: IconProps) => <svg {...S(p)}><polyline points="3 6 5 6 21 6"/><path d="M19 6l-1 14H6L5 6"/><path d="M10 11v6"/><path d="M14 11v6"/><path d="M9 6V4h6v2"/></svg>;
export const FolderOpenIcon   = (p: IconProps) => <svg {...S(p)}><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/><polyline points="9 14 12 17 15 14"/><line x1="12" y1="12" x2="12" y2="17"/></svg>;
export const PencilIcon       = (p: IconProps) => <svg {...S(p)}><path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"/></svg>;
export const SearchIcon       = (p: IconProps) => <svg {...S(p)}><circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/></svg>;
export const XIcon            = (p: IconProps) => <svg {...S(p)}><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>;
export const AlertTriangleIcon= (p: IconProps) => <svg {...S(p)}><path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>;
export const PackageIcon      = (p: IconProps) => <svg {...S(p)}><line x1="16.5" y1="9.4" x2="7.5" y2="4.21"/><path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z"/><polyline points="3.27 6.96 12 12.01 20.73 6.96"/><line x1="12" y1="22.08" x2="12" y2="12"/></svg>;
export const GripVerticalIcon = (p: IconProps) => <svg {...S(p)}><circle cx="9" cy="12" r="1"/><circle cx="9" cy="5" r="1"/><circle cx="9" cy="19" r="1"/><circle cx="15" cy="12" r="1"/><circle cx="15" cy="5" r="1"/><circle cx="15" cy="19" r="1"/></svg>;
export const Loader2Icon      = (p: IconProps) => <svg {...S(p)}><line x1="12" y1="2" x2="12" y2="6"/><line x1="12" y1="18" x2="12" y2="22"/><line x1="4.93" y1="4.93" x2="7.76" y2="7.76"/><line x1="16.24" y1="16.24" x2="19.07" y2="19.07"/><line x1="2" y1="12" x2="6" y2="12"/><line x1="18" y1="12" x2="22" y2="12"/><line x1="4.93" y1="19.07" x2="7.76" y2="16.24"/><line x1="16.24" y1="7.76" x2="19.07" y2="4.93"/></svg>;
export const CheckIcon        = (p: IconProps) => <svg {...S(p)}><polyline points="20 6 9 17 4 12"/></svg>;
export const UploadIcon       = (p: IconProps) => <svg {...S(p)}><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="17 8 12 3 7 8"/><line x1="12" y1="3" x2="12" y2="15"/></svg>;
export const ExternalLinkIcon = (p: IconProps) => <svg {...S(p)}><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/><polyline points="15 3 21 3 21 9"/><line x1="10" y1="14" x2="21" y2="3"/></svg>;
