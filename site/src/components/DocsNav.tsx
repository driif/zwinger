import { GlideNav, type GlideNavItem } from "@driif/motiv";

export default function DocsNav({ items, value }: { items: GlideNavItem[]; value: string }) {
  return <GlideNav label="Docs" items={items} value={value} onChange={(href) => location.assign(href)} />;
}
