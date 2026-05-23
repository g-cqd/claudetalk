import { createHash } from "node:crypto";

const ADJECTIVES = [
  "Swift", "Bright", "Calm", "Bold", "Wise", "Keen", "Sly", "Lone", "Wild", "Quiet",
  "Brave", "Clever", "Eager", "Fierce", "Gentle", "Happy", "Jolly", "Lucky", "Merry", "Noble",
  "Proud", "Quick", "Royal", "Sharp", "Silver", "Golden", "Crimson", "Azure", "Emerald", "Amber",
  "Frosty", "Stormy", "Sunny", "Misty", "Cosmic", "Iron", "Jade", "Velvet", "Mellow", "Vivid",
  "Hidden", "Restless", "Steady", "Patient", "Curious", "Daring", "Lucid", "Solar", "Lunar", "Polar",
  "Cinder", "Ember", "Onyx", "Sapphire", "Coral", "Pearl", "Hazel", "Ruby", "Indigo", "Scarlet",
  "Quirky", "Zesty", "Plucky", "Sleek", "Witty", "Stoic", "Spry", "Nimble", "Suave", "Snug",
] as const;

const ANIMALS = [
  "Fox", "Otter", "Hawk", "Wolf", "Lynx", "Owl", "Crow", "Bear", "Stag", "Eel",
  "Crane", "Heron", "Falcon", "Raven", "Salmon", "Marten", "Badger", "Boar", "Hare", "Mole",
  "Newt", "Toad", "Skink", "Viper", "Cobra", "Mantis", "Beetle", "Cicada", "Moth", "Wasp",
  "Whale", "Orca", "Seal", "Walrus", "Narwhal", "Dolphin", "Squid", "Octopus", "Crab", "Urchin",
  "Lion", "Tiger", "Leopard", "Panther", "Cheetah", "Jaguar", "Cougar", "Bobcat", "Caracal", "Serval",
  "Eagle", "Kite", "Osprey", "Vulture", "Condor", "Swan", "Goose", "Magpie", "Sparrow", "Finch",
  "Gecko", "Iguana", "Dragon", "Phoenix", "Griffin", "Kraken", "Sphinx", "Hydra", "Wyvern", "Roc",
] as const;

export interface Identity {
  pseudonym: string;
  path: string;
  hash: string;
  /** Ed25519 keypair for this (machine, folder). Lazily attached by the
   *  server startup; tools that don't sign (whoami, discover, inbox, …)
   *  treat it as optional. Always populated when called from src/server.ts
   *  main(); undefined only in unit-test stubs that build an Identity by
   *  hand. */
  keyPair?: import("./keys.ts").KeyPair;
}

/** Stable per-folder identity. SHA-256 of the absolute path drives the pick. */
export function pseudonymFor(absPath: string): Identity {
  const hash = createHash("sha256").update(absPath).digest("hex");
  const adjIdx = Number.parseInt(hash.slice(0, 8), 16) % ADJECTIVES.length;
  const aniIdx = Number.parseInt(hash.slice(8, 16), 16) % ANIMALS.length;
  const suffix = hash.slice(16, 19);
  return {
    pseudonym: `${ADJECTIVES[adjIdx]}${ANIMALS[aniIdx]}-${suffix}`,
    path: absPath,
    hash,
  };
}
