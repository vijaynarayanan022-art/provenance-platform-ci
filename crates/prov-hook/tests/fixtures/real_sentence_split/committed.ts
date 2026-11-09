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
	'u.s.',
	'u.k.',
	'u.n.',
	'usa.',
	'uk.'
]);

function normalizeToken(token: string): string {
	return token.replace(/[\])}"'’”]+$/g, '').replace(/\s+/g, '').toLowerCase();
}

function isAbbreviationCandidate(token: string): boolean {
	const normalized = normalizeToken(token);
	if (!normalized) {
		return false;
	}

	if (knownAbbreviations.has(normalized) || knownAbbreviations.has(`${normalized}.`)) {
		return true;
	}

	if (normalized.includes('.')) {
		const parts = normalized.split('.').filter(Boolean);
		if (parts.length > 1 && parts.every(part => /^[a-z]$/.test(part))) {
			return true;
		}
	}

	return false;
}

function isCommonAbbreviation(text: string, index: number): boolean {
	if (text[index] !== '.') {
		return false;
	}

	let prefixEnd = index;
	while (prefixEnd > 0 && /[A-Za-z.]/.test(text[prefixEnd - 1])) {
		prefixEnd--;
	}

	let suffixStart = index + 1;
	while (suffixStart < text.length && /[A-Za-z.]/.test(text[suffixStart])) {
		suffixStart++;
	}

	const candidate = `${text.slice(prefixEnd, index + 1)}${text.slice(index + 1, suffixStart)}`;
	if (!candidate || !/[A-Za-z.]/.test(candidate)) {
		return false;
	}

	return isAbbreviationCandidate(candidate);
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

		if (isDecimalPoint(trimmed, index) || isCommonAbbreviation(trimmed, index)) {
			continue;
		}

		const end = consumeSentenceEnd(trimmed, index);
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
