/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

/**
 * Normalizes whitespace by collapsing runs of whitespace to a single space and trimming the ends.
 */
export function normalizeWhitespace(text: string): string {
	return text.replace(/\s+/g, ' ').trim();
}

const ABBREVIATIONS = new Set([
	'dr',
	'mr',
	'mrs',
	'ms',
	'prof',
	'sr',
	'jr',
	'vs',
	'etc',
	'e.g',
	'i.e',
	'u.s',
	'u.k',
	'a.m',
	'p.m'
]);

function getTokenBefore(text: string, index: number): string {
	let start = index;
	while (start > 0 && /[A-Za-z0-9.]/.test(text[start - 1])) {
		start--;
	}
	return text.slice(start, index).trim();
}

function isDecimalPoint(text: string, index: number): boolean {
	if (text[index] !== '.') {
		return false;
	}

	const before = text.slice(0, index).trimEnd().slice(-1);
	const after = text.slice(index + 1).trimStart().slice(0, 1);
	return /\d/.test(before) && /\d/.test(after);
}

function isSentenceBoundary(text: string, index: number): boolean {
	const token = getTokenBefore(text, index).replace(/[\])\}"'”’]+$/g, '').trim();
	if (!token) {
		return true;
	}

	const normalized = token.toLowerCase();
	if (ABBREVIATIONS.has(normalized) || ABBREVIATIONS.has(`${normalized}.`)) {
		return false;
	}

	if (isDecimalPoint(text, index)) {
		return false;
	}

	return true;
}

/**
 * Splits text into sentences while avoiding false breaks after common abbreviations and inside quoted text.
 */
export function splitIntoSentences(text: string): string[] {
	const normalized = normalizeWhitespace(text);
	if (!normalized) {
		return [];
	}

	const sentences: string[] = [];
	let start = 0;

	for (let index = 0; index < normalized.length; index++) {
		const character = normalized[index];
		if (character !== '.' && character !== '!' && character !== '?') {
			continue;
		}

		if (!isSentenceBoundary(normalized, index)) {
			continue;
		}

		let end = index + 1;
		while (end < normalized.length && /[\)\]"'”’]/.test(normalized[end])) {
			end++;
		}

		const sentence = normalized.slice(start, end).trim();
		if (sentence) {
			sentences.push(sentence);
		}
		start = end;
		while (start < normalized.length && normalized[start] === ' ') {
			start++;
		}
		index = end;
	}

	const trailing = normalized.slice(start).trim();
	if (trailing) {
		sentences.push(trailing);
	}

	return sentences;
}

/**
 * Splits text into sentences and then tokenizes each sentence. Quoted phrases stay together as single tokens.
 */
export function tokenizeText(text: string): string[] {
	const sentences = splitIntoSentences(text);
	const tokens: string[] = [];
	for (const sentence of sentences) {
		const matches = sentence.match(/"[^"]+"|'[^']+'|[A-Za-z0-9]+(?:\.[A-Za-z0-9]+)*\.?|[A-Za-z0-9]+(?:[’'][A-Za-z0-9]+)*(?:-[A-Za-z0-9]+)*/g) ?? [];
		for (const match of matches) {
			const trimmed = match.trim();
			if (trimmed) {
				tokens.push(trimmed);
			}
		}
	}
	return tokens;
}

/**
 * Convenience alias for tokenizing text after sentence splitting.
 */
export const tokenize = tokenizeText;

/**
 * Returns the sentence fragments in a text value, treating common abbreviations as part of the sentence.
 */
export function splitTextIntoSentences(text: string): string[] {
	return splitIntoSentences(text);
}
