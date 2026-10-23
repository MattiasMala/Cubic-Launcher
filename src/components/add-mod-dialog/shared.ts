import type { ModrinthResult } from "../../lib/types";
import { MODRINTH_HEADERS } from "../../lib/modrinth-user-agent";
import type { LocalImportFailure } from "../../lib/contentImportErrors";
import { modrinthQueryLoaders } from "../../lib/pin-versions";

/**
 * What the Upload tab is uploading. `mod` keeps the local-JAR path; the other
 * three are content packs and go through the pack import command.
 */
export type LocalUploadContentType = "mod" | "resourcepack" | "datapack" | "shader";

/**
 * Which picker to open. A pack can be a `.zip` **or** a folder, and a Tauri
 * dialog opens files or directories, never both at once, so the choice is the
 * user's and it is explicit.
 */
export type LocalPickMode = "file" | "directory";

/** The four tabs of the Add dialog, as Modrinth's `project_type`. */
export type SearchContentType = "mod" | "resourcepack" | "datapack" | "shader";

export interface AddModDialogProps {
  /**
   * Both add callbacks resolve to `null` when the entry is on disk, or to the
   * reason it is not. The dialog marks a result «Added» only on `null`.
   */
  onAddModrinth: (id: string, name: string) => Promise<string | null>;
  onAddContent?: (contentType: string, id: string, name: string) => Promise<string | null>;
  /** Resolves to the refusal to show in the panel, or `null` when it worked. */
  onUploadLocal: (contentType: LocalUploadContentType, pick: LocalPickMode) => Promise<LocalImportFailure | null>;
  onDropLocal?: (path: string, contentType: LocalUploadContentType) => Promise<LocalImportFailure | null>;
}

interface ModrinthHit {
  slug: string;
  project_id: string;
  title: string;
  description: string;
  author: string;
  categories: string[];
  icon_url?: string;
  downloads: number;
}

export interface SearchFilters {
  categories: string[];
  loaders: string[];
  versions: string[];
  environments: string[];
  sortBy: string;
}

export const MOD_CATEGORIES = [
  "adventure", "decoration", "economy", "equipment", "food", "game-mechanics",
  "library", "magic", "management", "minigame", "mobs", "optimization",
  "social", "storage", "technology", "transportation", "utility", "worldgen",
];

// Le categorie e i loader di Modrinth sono per tipo di progetto
// (`/v2/tag/category`, `/v2/tag/loader`): un resource pack non ha mai
// «technology» né «fabric», e spuntarli svuotava la lista (audit 101, rete-6).
// I data pack usano le categorie dei mod.
const RESOURCEPACK_CATEGORIES = [
  "combat", "cursed", "decoration", "modded", "realistic", "simplistic", "themed",
  "tweaks", "utility", "vanilla-like",
  "audio", "blocks", "core-shaders", "entities", "environment", "equipment",
  "fonts", "gui", "items", "locale", "models",
  "8x-", "16x", "32x", "48x", "64x", "128x", "256x", "512x+",
];
const SHADER_CATEGORIES = [
  "cartoon", "cursed", "fantasy", "realistic", "semi-realistic", "vanilla-like",
  "atmosphere", "bloom", "colored-lighting", "foliage", "path-tracing", "pbr",
  "reflections", "shadows",
  "potato", "low", "medium", "high", "screenshot",
];

export const SEARCH_CATEGORIES: Record<SearchContentType, string[]> = {
  mod: MOD_CATEGORIES,
  datapack: MOD_CATEGORIES,
  resourcepack: RESOURCEPACK_CATEGORIES,
  shader: SHADER_CATEGORIES,
};

/** The loader checkboxes of each tab: none for resource and data packs, the shader loaders for shaders. */
export const SEARCH_LOADERS: Record<SearchContentType, string[]> = {
  mod: ["fabric", "quilt", "forge", "neoforge"],
  resourcepack: [],
  datapack: [],
  shader: ["iris", "optifine", "canvas"],
};

/** Client/server side only means something for mods. */
export const SEARCH_ENVIRONMENTS: Record<SearchContentType, string[]> = {
  mod: ["client", "server"],
  resourcepack: [],
  datapack: [],
  shader: [],
};

const LOADER_LABELS: Record<string, string> = {
  fabric: "Fabric",
  quilt: "Quilt",
  forge: "Forge",
  neoforge: "NeoForge",
  iris: "Iris",
  optifine: "OptiFine",
  canvas: "Canvas",
};

export function loaderLabel(loader: string): string {
  return LOADER_LABELS[loader] ?? loader.charAt(0).toUpperCase() + loader.slice(1);
}

export function categoryLabel(category: string): string {
  return category.replace(/-/g, " ").replace(/\b\w/g, c => c.toUpperCase());
}

/**
 * The Modrinth loaders a mod must declare to run on the mod list's loader
 * (D149). Vanilla has none to filter on. Every other loader asks what the
 * resolver and the pin ask (`modrinthQueryLoaders`, D148): NeoForge on 1.20.1
 * also takes the builds marked `forge`.
 */
export function compatibleSearchLoaders(modLoader: string, mcVersion: string): string[] {
  const loader = modLoader.trim().toLowerCase();
  if (!loader || loader === "vanilla") return [];
  return modrinthQueryLoaders(mcVersion, loader);
}

export const SORT_OPTIONS = [
  { value: "relevance", label: "Relevance" },
  { value: "downloads", label: "Downloads" },
  { value: "follows", label: "Follows" },
  { value: "updated", label: "Updated" },
  { value: "newest", label: "Newest" },
];

export function formatDownloads(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

/**
 * One page of Modrinth search. A failure throws: the dialog shows it as an
 * error, never as results (audit 101, todo.md:1486 — the old fallback showed
 * eight made-up mods that looked real). `signal` lets a newer search abort
 * this one (audit 101, rete-7).
 */
export async function searchModrinth(
  query: string,
  filters: SearchFilters,
  offset = 0,
  projectType = "mod",
  signal?: AbortSignal,
): Promise<{ results: ModrinthResult[]; totalHits: number }> {
  const facetGroups: string[][] = [[`project_type:${projectType}`]];
  if (filters.categories.length > 0) facetGroups.push(filters.categories.map(category => `categories:${category}`));
  // In the search index the loaders are categories too: `categories:<loader>`
  // is the facet Modrinth's own site sends (todo.md:1504).
  if (filters.loaders.length > 0) facetGroups.push(filters.loaders.map(loader => `categories:${loader}`));
  if (filters.versions.length > 0) facetGroups.push(filters.versions.map(version => `versions:${version}`));
  if (filters.environments.length > 0) {
    for (const env of filters.environments) {
      facetGroups.push([`${env}_side:required`, `${env}_side:optional`]);
    }
  }

  const params = new URLSearchParams();
  if (query.trim()) params.set("query", query);
  params.set("limit", "20");
  if (offset > 0) params.set("offset", String(offset));
  params.set("facets", JSON.stringify(facetGroups));
  params.set("index", (!query.trim() && filters.sortBy === "relevance") ? "downloads" : (filters.sortBy || "relevance"));

  const url = `https://api.modrinth.com/v2/search?${params}`;
  const res = await fetch(url, { headers: MODRINTH_HEADERS, signal });
  if (!res.ok) throw new Error(`Modrinth returned HTTP ${res.status}`);
  const data: { hits: ModrinthHit[]; total_hits: number } = await res.json();
  return {
    totalHits: data.total_hits,
    results: data.hits.map(hit => ({
      id: hit.slug || hit.project_id,
      name: hit.title,
      author: hit.author,
      description: hit.description,
      categories: hit.categories.slice(0, 3),
      iconUrl: hit.icon_url,
      downloads: hit.downloads,
    })),
  };
}
