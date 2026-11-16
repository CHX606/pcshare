import type { FsRoot } from '../../api/types'
import { formatSize } from '../../lib/format'
import { Icon, type IconName } from '../../components/Icon'

const ICON: Record<FsRoot['kind'], IconName> = {
  drive: 'drive',
  desktop: 'monitor',
  downloads: 'download',
  documents: 'file',
  home: 'folder',
}

/** “此电脑”：盘符和常用文件夹。 */
export function RootsView({ roots, onOpen, readOnly }: { roots: FsRoot[]; onOpen: (path: string) => void; readOnly: boolean }) {
  const drives = roots.filter((r) => r.kind === 'drive')
  const places = roots.filter((r) => r.kind !== 'drive')
  const tile = (r: FsRoot) => {
    const used = r.total && r.free != null ? 1 - r.free / r.total : null
    return (
      <li key={r.path}>
        <button
          type="button"
          disabled={readOnly}
          onClick={() => onOpen(r.path)}
          className="flex w-full items-center gap-3 rounded-lg border border-line bg-panel p-3 text-left hover:border-accent/50 hover:bg-hover disabled:opacity-60"
        >
          <Icon name={ICON[r.kind]} size={26} className={r.kind === 'drive' ? 'text-muted' : 'text-warn'} />
          <span className="min-w-0 flex-1">
            <span className="block truncate">{r.name}</span>
            {used != null && (
              <>
                <span className="mt-1.5 block h-1.5 overflow-hidden rounded-full bg-line">
                  <span className={`block h-full rounded-full ${used > 0.9 ? 'bg-bad' : 'bg-accent'}`} style={{ width: `${used * 100}%` }} />
                </span>
                <span className="mt-1 block text-xs text-muted">
                  可用 {formatSize(r.free!)}，共 {formatSize(r.total!)}
                </span>
              </>
            )}
            {used == null && <span className="block truncate text-xs text-muted">{r.path}</span>}
          </span>
        </button>
      </li>
    )
  }
  return (
    <div className="scroll-thin flex-1 overflow-y-auto p-5">
      <h3 className="mb-2 text-xs text-muted">常用文件夹</h3>
      <ul className="mb-6 grid grid-cols-[repeat(auto-fill,minmax(220px,1fr))] gap-2">{places.map(tile)}</ul>
      <h3 className="mb-2 text-xs text-muted">磁盘</h3>
      <ul className="grid grid-cols-[repeat(auto-fill,minmax(260px,1fr))] gap-2">{drives.map(tile)}</ul>
    </div>
  )
}
