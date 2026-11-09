/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { countSentences, countWords } from './wordStats.js';

/** A plain-language category for a Flesch reading-ease score. */
export type ReadingGrade = 'easy' | 'standard' | 'difficult';

/** A Flesch reading-ease score and its corresponding grade label. */
export interface ReadingLevel {
	score: number;
	grade: ReadingGrade;
}

const wordRegExp = /[\p{L}\p{N}]+(?:['\u2019][\p{L}\p{N}]+)*/gu;
const vowelGroupRegExp = /[aeiouy]+/g;

function estimateSyllables(word: string): number {
	const normalizedWord = word.toLowerCase();
	const vowelGroups = normalizedWord.match(vowelGroupRegExp);
	let syllables = vowelGroups?.length ?? 0;

	if (normalizedWord.length > 2 && /e$/.test(normalizedWord) && !/le$/.test(normalizedWord)) {
		syllables--;
	}

	return Math.max(1, syllables);
}

function countSyllables(text: string): number {
	let total = 0;
	for (const word of text.match(wordRegExp) ?? []) {
		total += estimateSyllables(word);
	}
	return total;
}

/** Maps a Flesch reading-ease score to easy (70+), standard (50-69), or difficult (below 50). */
export function getReadingGrade(score: number): ReadingGrade {
	if (score >= 70) {
		return 'easy';
	}
	if (score >= 50) {
		return 'standard';
	}
	return 'difficult';
}

/**
 * Computes the Flesch reading-ease score on a 0-100 scale. Syllables are estimated with an
 * English vowel-group heuristic, so the result is approximate and is best suited to English text.
 */
export function calculateFleschReadingEase(text: string): number {
	const wordCount = countWords(text);
	const sentenceCount = countSentences(text);
	if (wordCount === 0 || sentenceCount === 0) {
		return 0;
	}

	const averageWordsPerSentence = wordCount / sentenceCount;
	const averageSyllablesPerWord = countSyllables(text) / wordCount;
	const score = 206.835 - (1.015 * averageWordsPerSentence) - (84.6 * averageSyllablesPerWord);
	return Math.round(Math.max(0, Math.min(100, score)) * 10) / 10;
}

/** Calculates a Flesch reading-ease score and assigns its easy, standard, or difficult grade. */
export function analyzeReadingLevel(text: string): ReadingLevel {
	const score = calculateFleschReadingEase(text);
	return { score, grade: getReadingGrade(score) };
}
