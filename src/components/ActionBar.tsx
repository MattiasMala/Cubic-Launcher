import { For, Show, createSignal } from "solid-js";
import {
  setSelectedIds, search, setSearch,
  selectedCount, createAestheticGroup as createGroup, openIncompatibilityEditor,
  selectedTopLevelId, openAlternativesPanel, setFunctionalGroupModalOpen,
  functionalGroups, tagFilter, setTagFilter, toggleTagFilter,
  functionalGroupTagClass, functionalGroupTagStyle,
  addModToFunctionalGroup, selectedIds,
  openLinkModal, savedLinks, setLinksOverviewOpen,
} from "../store";
import { MaterialIcon, XIcon } from "./icons";

interface ActionBarProps {
  onAddMod: () => void;
  onDeleteSelected: () => void;
  // Creating a group can move its members next to each other: the new order is saved through these.
  onReorder?: (orderedIds: string[]) => void;
  onReorderAlts?: (parentId: string, orderedIds: string[]) => void;
}

export function ActionBar(props: ActionBarProps) {
  const [tagsOpen, setTagsOpen] = createSignal(false);

  /* ── Contextual bar (when mods selected) ──────────────────────── */
  const ContextualBar = () => (
    <header class="h-14 bg-primary/20 border-b border-primary flex items-center px-6 justify-between shrink-0">
      <div class="flex items-center gap-4">
        <span class="text-sm font-medium text-primary border-r border-primary/30 pr-4">
          {selectedCount()} mod{selectedCount() !== 1 ? "s" : ""} selected
        </span>
        <div class="flex items-center gap-2">
          <button
            onClick={() => createGroup({ onReorder: props.onReorder, onReorderAlts: props.onReorderAlts })}
            class="flex items-center gap-1.5 px-3 py-1.5 text-sm font-medium text-primary hover:text-white hover:bg-primary/40 rounded-lg border border-primary/30 transition-colors duration-75"
          >
            <MaterialIcon name="folder_open" size="md" />
            Create Group
          </button>
          <button
            onClick={props.onDeleteSelected}
            class="flex items-center gap-1.5 px-3 py-1.5 text-sm font-medium text-primary hover:text-white hover:bg-primary/40 rounded-lg border border-primary/30 transition-colors duration-75"
          >
            <MaterialIcon name="delete" size="md" />
            Delete
          </button>
          {(() => {
            const [tagMenuOpen, setTagMenuOpen] = createSignal(false);
            return (
              <div class="relative">
                <button
                  onClick={() => setTagMenuOpen(o => !o)}
                  class="flex items-center gap-1.5 px-3 py-1.5 text-sm font-medium text-primary hover:text-white hover:bg-primary/40 rounded-lg border border-primary/30 transition-colors duration-75"
                >
                  <MaterialIcon name="folder_special" size="md" />
                  Add Tag
                  <MaterialIcon name={tagMenuOpen() ? "expand_less" : "expand_more"} size="sm" />
                </button>
                <Show when={tagMenuOpen()}>
                  <div class="fixed inset-0 z-40" onClick={() => setTagMenuOpen(false)} />
                  <div class="absolute left-0 top-full mt-1 z-50 min-w-[180px] bg-bgPanel border border-borderColor rounded-lg shadow-lg overflow-hidden">
                    {/* Create new tag */}
                    <button
                      onClick={() => { setTagMenuOpen(false); setFunctionalGroupModalOpen(true); }}
                      class="w-full flex items-center gap-2 px-3 py-2 text-sm text-primary hover:bg-primary/10 transition-colors border-b border-borderColor"
                    >
                      <MaterialIcon name="add" size="sm" />
                      Create New Tag
                    </button>
                    {/* Existing tags */}
                    <For each={functionalGroups()}>
                      {g => (
                        <button
                          onClick={() => {
                            for (const id of selectedIds()) addModToFunctionalGroup(g.id, id);
                            setTagMenuOpen(false);
                          }}
                          class="w-full flex items-center gap-2 px-3 py-2 text-sm hover:bg-muted/20 transition-colors"
                        >
                          <span class={functionalGroupTagClass(g.tone)} style={functionalGroupTagStyle(g.tone)}>{g.name}</span>
                        </button>
                      )}
                    </For>
                    <Show when={functionalGroups().length === 0}>
                      <div class="px-3 py-2 text-xs text-muted-foreground">No tags yet.</div>
                    </Show>
                  </div>
                </Show>
              </div>
            );
          })()}
          <button
            onClick={openLinkModal}
            disabled={selectedCount() < 2}
            class="flex items-center gap-1.5 px-3 py-1.5 text-sm font-medium text-primary hover:text-white hover:bg-primary/40 rounded-lg border border-primary/30 transition-colors duration-75 disabled:opacity-50 disabled:cursor-not-allowed"
            title={selectedCount() >= 2 ? "Link selected mods" : "Select 2 or more mods to link"}
          >
            <MaterialIcon name="link" size="md" />
            Link
          </button>
          <button
            onClick={openIncompatibilityEditor}
            disabled={selectedCount() !== 1}
            class="flex items-center gap-1.5 px-3 py-1.5 text-sm font-medium text-primary hover:text-white hover:bg-primary/40 rounded-lg border border-primary/30 transition-colors duration-75 disabled:opacity-50 disabled:cursor-not-allowed"
            title={selectedCount() === 1 ? "Manage incompatibilities" : "Select exactly one mod"}
          >
            <MaterialIcon name="warning" size="md" />
            Incompatibilities
          </button>
          <button
            onClick={() => {
              const pid = selectedTopLevelId();
              if (pid) openAlternativesPanel(pid);
            }}
            disabled={!selectedTopLevelId()}
            class="flex items-center gap-1.5 px-3 py-1.5 text-sm font-medium text-primary hover:text-white hover:bg-primary/40 rounded-lg border border-primary/30 transition-colors duration-75 disabled:opacity-50 disabled:cursor-not-allowed"
            title={selectedTopLevelId() ? "Manage fallback alternatives" : "Select exactly one mod"}
          >
            <MaterialIcon name="account_tree" size="md" />
            Alternatives
          </button>
        </div>
      </div>
      <button
        onClick={() => setSelectedIds([])}
        class="text-primary hover:text-white p-1 transition-colors duration-75"
      >
        <MaterialIcon name="close" size="lg" />
      </button>
    </header>
  );

  /* ── Normal bar ───────────────────────────────────────────────── */
  const hasActiveFilters = () => tagFilter().size > 0;

  const NormalBar = () => (
    <div class="px-6 py-2 bg-bgPanel border-b border-borderColor shrink-0 flex items-center justify-between gap-3">
      {/* Left: Add Mods + Tags dropdown + Links + Clear */}
      <div class="flex items-center gap-2 flex-wrap">
        {/* Add Mods */}
        <button
          onClick={props.onAddMod}
          class="px-4 py-1.5 rounded-lg bg-primary hover:bg-brandPurpleHover text-white text-sm font-medium flex items-center gap-2 transition-colors duration-75"
        >
          <MaterialIcon name="add" size="md" />
          Add Mods
        </button>

        {/* Tags dropdown */}
        <Show when={functionalGroups().length > 0}>
          <div class="relative">
            <button
              onClick={() => setTagsOpen(o => !o)}
              class={`flex items-center gap-1 px-2 py-0.5 rounded-md border text-xs transition-colors duration-75 ${
                tagFilter().size > 0
                  ? "border-primary bg-primary/10 text-primary"
                  : "border-borderColor bg-bgDark text-textMuted hover:text-textMain"
              }`}
            >
              <MaterialIcon name="label" size="sm" />
              Tags
              <Show when={tagFilter().size > 0}>
                <span class="font-medium">({tagFilter().size})</span>
              </Show>
              <MaterialIcon name={tagsOpen() ? "expand_less" : "expand_more"} size="sm" />
            </button>
            <Show when={tagsOpen()}>
              {/* Click-outside backdrop */}
              <div class="fixed inset-0 z-40" onClick={() => setTagsOpen(false)} />
              <div class="absolute left-0 top-full mt-1 z-50 min-w-[160px] bg-bgPanel border border-borderColor rounded-lg shadow-lg overflow-hidden">
                <For each={functionalGroups()}>
                  {g => {
                    const active = () => tagFilter().has(g.id);
                    return (
                      <button
                        onClick={() => toggleTagFilter(g.id)}
                        class={`w-full flex items-center justify-between gap-2 px-3 py-2 text-sm transition-colors duration-75 ${
                          active() ? "bg-primary/10" : "hover:bg-muted/20"
                        }`}
                      >
                        <span class={functionalGroupTagClass(g.tone)} style={functionalGroupTagStyle(g.tone)}>
                          {g.name}
                        </span>
                        <Show when={active()}>
                          <MaterialIcon name="check" size="sm" class="text-primary shrink-0" />
                        </Show>
                      </button>
                    );
                  }}
                </For>
              </div>
            </Show>
          </div>
        </Show>

        {/* Links overview button */}
        <Show when={savedLinks().length > 0}>
          <button
            onClick={() => setLinksOverviewOpen(true)}
            class="flex items-center gap-1 px-2 py-0.5 rounded-md border border-cyan-500/30 bg-cyan-500/10 text-cyan-300 text-xs hover:bg-cyan-500/20 transition-colors duration-75"
            title="View all link relations"
          >
            <MaterialIcon name="link" size="sm" />
            Links ({savedLinks().length})
          </button>
        </Show>

        {/* Clear the tag filter */}
        <Show when={hasActiveFilters()}>
          <button
            onClick={() => setTagFilter(new Set<string>())}
            class="flex items-center gap-1 px-2 py-0.5 rounded-md text-xs text-textMuted hover:text-white hover:bg-muted/40 border border-dashed border-borderColor transition-colors duration-75"
            title="Clear the tag filter"
          >
            <XIcon class="h-3 w-3" />
            Clear
          </button>
        </Show>
      </div>

      {/* Right: Search */}
      <div class="flex-1 max-w-md relative">
        <MaterialIcon name="search" size="md" class="absolute left-3 top-1/2 -translate-y-1/2 text-textMuted" />
        <input
          type="text"
          placeholder="Search installed mods..."
          value={search()}
          onInput={e => setSearch(e.currentTarget.value)}
          class="w-full bg-bgDark border border-borderColor rounded-lg py-1.5 pl-9 pr-3 text-sm text-textMain placeholder-textMuted focus:outline-none focus:border-primary focus:ring-1 focus:ring-primary transition-colors duration-75"
        />
        <Show when={search()}>
          <button
            onClick={() => setSearch("")}
            class="absolute right-2 top-1/2 -translate-y-1/2 text-textMuted hover:text-white transition-colors"
          >
            <XIcon class="h-4 w-4" />
          </button>
        </Show>
      </div>
    </div>
  );

  return (
    <Show when={selectedCount() > 0} fallback={<NormalBar />}>
      <ContextualBar />
    </Show>
  );
}
