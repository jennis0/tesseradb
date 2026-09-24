import {useCallback, useEffect, useState} from 'react';
import {useProjection, useTesseraStore} from '@tesseradb/react';
import {TesseraExplorer} from '@tesseradb/react/components';
import {ItemCard} from './ItemCard.js';

/**
 * The explorer in React 19 through the wrappers, with the `detail` slot holding a host component
 * that reads `useProjection` in place of `<tessera-item-card>`.
 *
 * The host builds the store with `useTesseraStore` and passes it to the explorer as a property,
 * so the host's components and the explorer read one store. The token comes from the app server
 * (`../plain-html/server.mjs`) through `authorise`, which the store calls to renew.
 */
type User = {name: string; label: string};

export function App() {
  const [users, setUsers] = useState<User[]>([]);
  const [user, setUser] = useState<string>('');
  useEffect(() => {
    void fetch('/users')
      .then((r) => r.json() as Promise<User[]>)
      .then((list) => {
        setUsers(list);
        setUser(list.at(-1)?.name ?? '');
      });
  }, []);
  return (
    <div style={{display: 'grid', gridTemplateRows: 'auto 1fr', height: '100%'}}>
      <header style={{display: 'flex', gap: '1rem', alignItems: 'center', padding: '0.5rem 1rem', borderBottom: '1px solid #ddd'}}>
        <strong>Tessera</strong>
        <label>
          signed in as{' '}
          <select id="principal" value={user} onChange={(e) => setUser(e.target.value)}>
            {users.map((u) => (
              <option key={u.name} value={u.name}>
                {u.label}
              </option>
            ))}
          </select>
        </label>
      </header>
      {user ? <Session key={user} user={user} /> : null}
    </div>
  );
}

/** One session: keyed by user above, so a different user unmounts this and its store with it. */
function Session({user}: {user: string}) {
  const authorise = useCallback(async () => {
    const r = await fetch(`/token?user=${encodeURIComponent(user)}`, {method: 'POST'});
    if (!r.ok) throw new Error(`token: ${r.status}`);
    return (await r.json()) as {token: string; expiresAt: number};
  }, [user]);
  const store = useTesseraStore({viewerUrl: location.origin, authorise});
  const status = useProjection(store, 'status');
  if (!store) return null;
  return (
    <TesseraExplorer store={store} layout="overlay" style={{minHeight: 0}} title={status?.status}>
      <div slot="detail">
        <ItemCard store={store} />
      </div>
    </TesseraExplorer>
  );
}
