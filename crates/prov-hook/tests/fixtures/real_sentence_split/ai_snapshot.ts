/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

const sentenceEndPattern = /[.!?]/;

const knownAbbreviations = new Set([
	'dr.',
	'mr.',
	'mrs.',
	'ms.',
	'prof.',
	'sr.',
	'jr.',
	'st.',
	'vs.',
	'etc.',
	'eg.',
	'e.g.',
	'i.e.',
	'no.',
	'fig.',
	'jan.',
	'feb.',
	'mar.',
	'apr.',
	'may.',
	'jun.',
	'jul.',
	'aug.',
	'sep.',
	'oct.',
	'nov.',
	'dec.',
	'a.m.',
	'p.m.',
	'vs.',
	'u.s.',
	'u.k.',
	'u.n.',
	'usa.',
	'uk.'
]);

function getTrailingToken(text: string): string {
	const match = /([A-Za-z][A-Za-z0-9.'-]*)$/.exec(text.trimEnd());
	return match ? match[1] : '';
}

function normalizeToken(token: string): string {
	return token.replace(/[\])}"'’”]+$/g, '').replace(/[\s]+/g, '').toLowerCase();
}

function looksLikeInitialism(token: string): boolean {
	const normalized = normalizeToken(token);
	if (!normalized.includes('.')) {
		return false;
	}

	const parts = normalized.split('.').filter(Boolean);
	return parts.length > 1 && parts.every(part => /^[a-z]$/.test(part));
}

function isCommonAbbreviation(token: string): boolean {
	const normalized = normalizeToken(token);
	if (!normalized) {
		return false;
	}

	if (knownAbbreviations.has(normalized)) {
		return true;
	}

	if (normalized.endsWith('.')) {
		const withoutTrailingDot = normalized.slice(0, -1);
		if (knownAbbreviations.has(withoutTrailingDot)) {
			return true;
		}
	}

	if (normalized.includes('.')) {
		const parts = normalized.split('.').filter(Boolean);
		if (parts.length > 1 && parts.every(part => /^[a-z]{1,3}$/.test(part))) {
			return true;
		}
	}

	return looksLikeInitialism(token);
}

function isDecimalPoint(text: string, index: number): boolean {
	if (text[index] !== '.') {
		return false;
	}

	const previous = text.slice(0, index).trimEnd().slice(-1);
	const next = text.slice(index + 1).trimStart().slice(0, 1);
	return /^[0-9]$/.test(previous) && /^[0-9]$/.test(next);
}

function consumeSentenceEnd(text: string, index: number): number {
	let end = index;
	while (end < text.length && sentenceEndPattern.test(text[end])) {
		end++;
	}

	while (end < text.length && /['"”’\])}]/.test(text[end])) {
		end++;
	}

	return end;
}

/**
 * Splits plain text into sentences while avoiding false breaks inside common abbreviations such as
 * "Dr.", "e.g.", and "U.S.".
 *
 * The returned sentences retain their trailing punctuation and surrounding whitespace is trimmed.
 */
export function splitSentences(text: string): string[] {
	const trimmed = text.trim();
	if (!trimmed) {
		return [];
	}

	const sentences: string[] = [];
	let sentenceStart = 0;

	for (let index = 0; index < trimmed.length; index++) {
		const current = trimmed[index];
		if (!sentenceEndPattern.test(current)) {
			continue;
		}

		if (isDecimalPoint(trimmed, index)) {
			continue;
		}

		const end = consumeSentenceEnd(trimmed, index);
		const beforePunctuation = trimmed.slice(0, index).trimEnd();
		const token = getTrailingToken(beforePunctuation);
		if (token && isCommonAbbreviation(token)) {
			index = end - 1;
			continue;
		}

		const sentence = trimmed.slice(sentenceStart, end).trim();
		if (sentence) {
			sentences.push(sentence);
		}
		sentenceStart = end;
		index = end - 1;
	}

	const trailing = trimmed.slice(sentenceStart).trim();
	if (trailing) {
		sentences.push(trailing);
	}

	return sentences;
}

/**
 * Alias for `splitSentences`.
 */
export const sentenceSplit = splitSentences;
