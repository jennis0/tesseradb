import {useProjection, type Store} from '@tesseradb/react';

/**
 * A host's own item card: the selected item's record from the `selection` projection, its
 * fields **by name in declaration order** from `meta` — `/v1/items` omits a field the item has
 * no value for, so position lies. A refusal is shown as one, never as an empty item.
 */
export function ItemCard({store}: {store: Store}) {
  const selection = useProjection(store, 'selection');
  const meta = useProjection(store, 'meta');
  const box: React.CSSProperties = {padding: '0.6rem 1rem', background: '#fff', border: '1px solid #ddd', borderRadius: 6};
  if (selection.itemRefusal) {
    return (
      <section style={box} data-state="refused">
        <h2 style={{margin: 0, fontSize: '1rem'}}>Item</h2>
        <p>refused — {selection.itemRefusal.code}: {selection.itemRefusal.detail}</p>
      </section>
    );
  }
  if (!selection.item) {
    return (
      <section style={box} data-state="empty">
        <h2 style={{margin: 0, fontSize: '1rem'}}>Item</h2>
        <p style={{opacity: 0.6}}>click a mark</p>
      </section>
    );
  }
  const {fields} = selection.item.detail;
  const declared = (meta?.declaredScalars ?? []).map((c) => c.name);
  const names = [...declared.filter((n) => n in fields), ...Object.keys(fields).filter((n) => !declared.includes(n))];
  return (
    <section style={box} data-state="shown">
      <h2 style={{margin: 0, fontSize: '1rem'}}>Item {selection.item.id.toString()}</h2>
      <dl style={{display: 'grid', gridTemplateColumns: 'auto 1fr', gap: '0.2rem 1rem', margin: '0.5rem 0 0'}}>
        {names.map((name) => (
          <Field key={name} name={name} value={fields[name]} />
        ))}
      </dl>
    </section>
  );
}

function Field({name, value}: {name: string; value: unknown}) {
  const text = value === null || value === undefined ? '—' : String(value);
  return (
    <>
      <dt style={{opacity: 0.6}}>{name}</dt>
      <dd style={{margin: 0, overflowWrap: 'anywhere'}}>{text.length > 400 ? `${text.slice(0, 400)}…` : text}</dd>
    </>
  );
}
