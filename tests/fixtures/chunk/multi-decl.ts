// Shared configuration for the inventory service.
import { readFile } from "fs";

/** The default page size for listings. */
export const PAGE_SIZE = 50; // tuned against the slowest client

// A stock item as stored on disk.
export interface Item {
  // The stable identifier.
  id: string;
  quantity: number; // never negative
}

/* Formats an item for logs. */
export function describe(item: Item): string {
  // Quantity first, so columns line up.
  return `${item.quantity} x ${item.id}`;
}

// Totals the quantities of all items.
export function total(items: Item[]): number {
  return items.reduce((sum, item) => sum + item.quantity, 0); // empty is zero
}

export class Store {
  // Items keyed by id.
  private items = new Map<string, Item>();

  // Adds or replaces an item.
  put(item: Item): void {
    this.items.set(item.id, item);
  }

  // Looks an item up by id.
  get(id: string): Item | undefined {
    return this.items.get(id);
  }
}

// Loads a store from a JSON file.
export function load(path: string, done: (store: Store) => void): void {
  readFile(path, "utf8", (error, data) => {
    // A missing file is an empty store.
    const store = new Store();
    if (!error) {
      for (const item of JSON.parse(data) as Item[]) store.put(item);
    }
    done(store);
  });
}

type Pair = [string, number]; /* id and quantity */

// Turns items into pairs for export.
export const pairs = (items: Item[]): Pair[] =>
  items.map((item) => [item.id, item.quantity]);

// End of the module: nothing below is exported.
// Keep this note last.
