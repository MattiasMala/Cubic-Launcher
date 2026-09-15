export type ContentVersionRule = { kind: string; mcVersions: string[]; loader: string };

export type ContentEntry = {
  id: string;
  source: string;
  versionRules: ContentVersionRule[];
  /**
   * Display data a locally imported pack carries in its own snapshot
   * (`ContentEntrySnapshot`, `content_packs.rs:177`): the name comes from the
   * file or folder that was imported, the icon is the `pack.png` served as a
   * `data:image/png;base64,…`. Modrinth entries leave both empty and keep
   * taking title and icon from the API.
   */
  name?: string;
  iconImage?: string;
};

export type ContentGroupData = {
  id: string;
  name: string;
  collapsed: boolean;
  entryIds: string[];
};

/**
 * The wire shape of `load_content_list_command` (`ContentSnapshot`,
 * `content_packs.rs:158-185`), kept apart from `ContentEntry` above: that one
 * is what the components consume, this one is what the backend sends.
 */
export type ContentSnapshotPayload = {
  entries?: Array<{
    id: string;
    source: string;
    versionRules?: ContentVersionRule[];
    name?: string | null;
    fileName?: string | null;
    iconImage?: string | null;
    description?: string | null;
  }>;
  groups?: ContentGroupData[];
};

export type ContentMeta = {
  name: string;
  iconUrl?: string;
};

export type ContentTopLevelItem =
  | { kind: "entry"; entry: ContentEntry }
  | { kind: "group"; id: string; name: string; collapsed: boolean; entries: ContentEntry[] };

export interface ContentTabViewProps {
  type: string;
  modlistName: string;
  onAddContent: () => void;
}

export const CONTENT_TAB_LABELS: Record<string, string> = {
  resourcepack: "Resource Packs",
  datapack: "Data Packs",
  shader: "Shaders",
};
